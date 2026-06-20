use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::adoption_analytics::build_runtime_adoption_analytics;
use crate::brief::brief_from_context;
use crate::context::assemble_context_with_vector;
use crate::core::{
    AsyncDb,
    anchor::{self, DerivedAnchor},
    config::{Config, ConfigHandle},
    db::{
        CURRENT_VECTOR_INDEX_VERSION, Database, NoveltyAuditInsert, VECTOR_DISTANCE_METRIC,
        read_fork_ext_version,
    },
    phase3::{
        EvaluatorAdviceInput, RuntimeAdoptionCaptureInput, RuntimeAdoptionCheckedRecordReport,
        RuntimeAdoptionRecordPlanInput, RuntimeAdoptionReviewFilters,
        build_research_ingest_plan_from_value, capture_runtime_adoption_record_input,
        card_context_default_proposal, card_context_default_readiness,
        card_context_rollback_control, check_runtime_adoption_record, evaluator_advice,
        prepare_runtime_adoption_capture, prepare_runtime_adoption_record,
        review_runtime_adoption_events, runtime_adoption_guidance,
        runtime_adoption_instrumentation_policy, should_write_checked_record,
    },
    project::{ProjectSearchScope, infer_project_id_from_root_uri, validate_project_id},
    queue::{AsyncPendingMessageStore, ClaimedMessage, PendingMessageStore},
    reindex::ReindexProgressStore,
    remote_calls::{
        RemoteCallService, endpoint_policy_diagnostic_label, endpoint_policy_global_runtime_error,
        endpoint_policy_runtime_error,
    },
    strata::{count_raw_turn_drawers, is_raw_turn, raw_turn_importance, should_store_raw_turns},
    types::{
        AnchorKind, BootstrapIdentityParts, Drawer, DrawerSummary, ExplicitTunnel,
        KnowledgeCardFilter, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, Provenance,
        RuntimeAdoptionEvent, RuntimeAdoptionFilter, RuntimeAdoptionSignal, RuntimeAdoptionTrack,
        SearchResult, SourceType, TriggerHints, Triple, default_confidence,
    },
    utils::{
        build_bootstrap_drawer_id_from_parts, build_triple_id, current_timestamp, expand_home,
        iso_timestamp, knowledge_source_file, link_superseded_drawer, normalize_rfc3339_timestamp,
        source_file_or_synthetic,
    },
};
use crate::cowork::{
    AgentRecord, AgentStatus, BusError, DeliveryReport, InboxMessage, PeekError,
    PeekRequest as CoworkPeekRequest, Tool, peek_partner,
};
use crate::doctor::{COWORK_BUS_ACTIONS, PHASE3_ACTIONS, REQUIRED_MCP_TOOLS, build_doctor_report};
use crate::embed::{EmbedderFactory, global_embed_status};
use crate::field_taxonomy::field_taxonomy;
use crate::ingest::{
    IngestError,
    gating::{
        GatingDecision, GatingRuntime, IngestCandidate, evaluate_fact_check_gate, evaluate_tier1,
        evaluate_tier2, should_route_to_llm_judge,
    },
    normalize::CURRENT_NORMALIZE_VERSION,
    novelty::{NoveltyAction, NoveltyCandidate, evaluate as evaluate_novelty},
};
use crate::knowledge_anchor::{PublishAnchorRequest as CorePublishAnchorRequest, publish_anchor};
use crate::knowledge_card_lifecycle::{
    DemoteCardRequest as CoreDemoteCardRequest, PromoteCardRequest as CorePromoteCardRequest,
    demote_card, evaluate_card_gate_by_id, promote_card,
};
use crate::knowledge_card_retrieval::{
    KnowledgeCardRetrievalRequest as CoreCardRetrievalRequest, retrieve_knowledge_cards_with_vector,
};
use crate::knowledge_distill::{
    DistillPlan, DistillRequest as CoreDistillRequest, commit_distill, prepare_distill,
};
use crate::knowledge_gate::{evaluate_gate_by_id, promotion_policy};
use crate::knowledge_lifecycle::{
    DemoteRequest as CoreDemoteRequest, PromoteRequest as CorePromoteRequest, demote_knowledge,
    promote_knowledge,
};
use crate::search::{
    SearchFilters, SearchMode, SearchOptions, VectorSearchCircuit, bm25_fallback_warning_degraded,
    bm25_fallback_warning_embed_error, bm25_fallback_warning_timeout, dispatch_access_update,
    maybe_rerank_search_results, resolve_route, search_bm25_only_with_options,
    search_with_vector_and_scope_options,
};
use anyhow::Context;
use rmcp::{
    ErrorData, Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    service::Peer,
    tool, tool_handler, tool_router,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::OnceCell;

use super::timeline::{TimelineRequest, TimelineResponse};
use super::tools::{
    BriefMcpRequest, BriefMcpResponse, ChunkerStatsDto, ContextRequest, ContextResponse,
    CoworkBusAgentDto, CoworkBusCaptureDto, CoworkBusChannelDto, CoworkBusDeliveryDto,
    CoworkBusDeliveryStatusDto, CoworkBusDoctorDto, CoworkBusEventDto, CoworkBusHandoffAgentDto,
    CoworkBusHandoffDto, CoworkBusHandoffFiltersDto, CoworkBusMessageDto, CoworkBusRequest,
    CoworkBusResponse, CoworkBusSessionDto, CoworkBusTmuxPeekDto, CoworkBusTmuxProbeDto,
    CoworkPushRequest, CoworkPushResponse, DatabaseDiagnosticDto, DeleteRequest, DeleteResponse,
    DesignInsightStatusDto, DoctorMcpDto, DoctorRequest, DoctorResponse, DoctorToolDto,
    DuplicateWarning, EmbedEndpointStatusDto, EmbedStatusDto, EmbedderCircuitDto,
    EndpointHealthDto, FactCheckRequest, FactCheckResponse, FieldTaxonomyEntryDto,
    FieldTaxonomyResponse, GatingRuntimeStatusDto, IngestControls, IngestOperationState,
    IngestRequest, IngestResponse, IntelligenceStatusDto, KgRequest, KgResponse, KgStatsDto,
    KnowledgeCardDto, KnowledgeCardEventDto, KnowledgeCardsRequest, KnowledgeCardsResponse,
    KnowledgeDemoteRequest, KnowledgeDemoteResponse, KnowledgeDistillRequest,
    KnowledgeDistillResponse, KnowledgeGateRequest, KnowledgeGateResponse, KnowledgePolicyResponse,
    KnowledgePromoteRequest, KnowledgePromoteResponse, KnowledgePublishAnchorRequest,
    KnowledgePublishAnchorResponse, LeaseInfoDto, LeaseRequest, LeaseResponse,
    LlmEndpointStatusDto, LlmStatusDto, MAX_READ_DRAWERS_MAX_COUNT, MAX_READ_DRAWERS_REQUEST_IDS,
    OperationStatusRequest, PeekMessageDto, PeekPartnerRequest, PeekPartnerResponse, Phase3GateDto,
    Phase3Request, Phase3Response, PinnedFactDto, PinnedFactProjectCount, PinnedFactsRequest,
    PinnedFactsResponse, QueueStatsDto, ReadDrawerRequest, ReadDrawerResponse, ReadDrawersRequest,
    ReadDrawersResponse, ResearchAdapterPlanDto, ResearchIngestPlanDto, RetrievalScopeRequest,
    RetrievedKnowledgeCardDto, RollbackRequest, RollbackResponse, RuntimeAdoptionEventDto,
    RuntimeAdoptionStatsDto, ScopeCount, ScrubStatsDto, SearchRequest, SearchResponse,
    SearchResultDto, SkillDto, SkillRequest, SkillResponse, SkillSummaryDto, SourceTypeCount,
    StatusDetail, StatusRequest, StatusResponse, StatusScope, SystemWarning, TaxonomyEntryDto,
    TaxonomyRequest, TaxonomyResponse, TriggerHintsDto, TripleDto, TunnelDto, TunnelEndpointDto,
    TunnelsRequest, TunnelsResponse, TurnStorageStatusDto,
};

fn config_db_path_matches_server(config: &Config, server_db_path: &Path) -> bool {
    let config_db_path = expand_home(&config.db_path);
    if config_db_path == server_db_path {
        return true;
    }
    match (config_db_path.canonicalize(), server_db_path.canonicalize()) {
        (Ok(config_db_path), Ok(server_db_path)) => config_db_path == server_db_path,
        _ => false,
    }
}

const MCP_SEARCH_ROUTE_DEADLINE: Duration = Duration::from_secs(5);
const MCP_SEARCH_DB_DEADLINE: Duration = Duration::from_secs(30);
const MCP_SEARCH_STALE_INDEX_DEADLINE: Duration = Duration::from_secs(2);
const MCP_INGEST_ADMISSION_DEADLINE: Duration = Duration::from_secs(10);
const MCP_OPERATION_STATUS_DEADLINE: Duration = Duration::from_secs(5);

fn mcp_ingest_idempotency_key(payload: &str) -> String {
    let now_ns = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => 0,
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mempal mcp ingest admission v1");
    hasher.update(&[0]);
    hasher.update(&now_ns.to_le_bytes());
    hasher.update(&[0]);
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&[0]);
    hasher.update(payload.as_bytes());
    format!("mcp-ingest-{}", hasher.finalize().to_hex())
}

#[derive(Clone)]
pub struct MempalMcpServer {
    db_path: PathBuf,
    initial_config: Arc<Config>,
    async_db: Arc<OnceCell<AsyncDb>>,
    async_queue: AsyncPendingMessageStore,
    gating_runtime: Arc<GatingRuntime>,
    embedder_factory: Arc<dyn EmbedderFactory>,
    tool_router: ToolRouter<Self>,
    /// Captured via `initialize` override so `auto` peek mode can infer the
    /// partner from the calling MCP client's self-reported name.
    client_name: Arc<Mutex<Option<String>>>,
    client_project_id: Arc<Mutex<Option<String>>>,
    client_peer: Arc<Mutex<Option<Peer<rmcp::RoleServer>>>>,
    /// Per-session drawer IDs that were returned by `mempal_search`.
    /// Flushed and boosted on the next `mempal_ingest` call (P13).
    session_hit_drawers: Arc<Mutex<HashSet<String>>>,
    ingest_worker_started: Arc<AtomicBool>,
    search_route_deadline: Duration,
    search_db_deadline: Duration,
    search_stale_index_deadline: Duration,
    ingest_admission_deadline: Duration,
    operation_status_deadline: Duration,
    #[cfg(any(test, feature = "db-test-seam"))]
    ingest_processing_delay: Option<Duration>,
}

impl MempalMcpServer {
    pub fn new(db_path: PathBuf, config: crate::core::config::Config) -> anyhow::Result<Self> {
        Self::new_with_factory_and_config(
            db_path,
            config.clone(),
            Arc::new(crate::embed::ConfiguredEmbedderFactory::new(config)),
        )
    }

    pub fn new_with_factory(
        db_path: PathBuf,
        embedder_factory: Arc<dyn EmbedderFactory>,
    ) -> anyhow::Result<Self> {
        Self::new_with_factory_and_config(
            db_path,
            ConfigHandle::current().as_ref().clone(),
            embedder_factory,
        )
    }

    pub fn new_with_factory_and_config(
        db_path: PathBuf,
        config: Config,
        embedder_factory: Arc<dyn EmbedderFactory>,
    ) -> anyhow::Result<Self> {
        let async_queue = AsyncPendingMessageStore::new_without_reclaim(&db_path);
        let initial_config = Arc::new(config.clone());
        Ok(Self {
            db_path,
            initial_config,
            async_db: Arc::new(OnceCell::new()),
            async_queue,
            gating_runtime: Arc::new(GatingRuntime::new(config, Arc::clone(&embedder_factory))),
            embedder_factory,
            tool_router: Self::tool_router(),
            client_name: Arc::new(Mutex::new(None)),
            client_project_id: Arc::new(Mutex::new(None)),
            client_peer: Arc::new(Mutex::new(None)),
            session_hit_drawers: Arc::new(Mutex::new(HashSet::new())),
            ingest_worker_started: Arc::new(AtomicBool::new(false)),
            search_route_deadline: MCP_SEARCH_ROUTE_DEADLINE,
            search_db_deadline: MCP_SEARCH_DB_DEADLINE,
            search_stale_index_deadline: MCP_SEARCH_STALE_INDEX_DEADLINE,
            ingest_admission_deadline: MCP_INGEST_ADMISSION_DEADLINE,
            operation_status_deadline: MCP_OPERATION_STATUS_DEADLINE,
            #[cfg(any(test, feature = "db-test-seam"))]
            ingest_processing_delay: None,
        })
    }

    fn status_config_snapshot(&self) -> Arc<Config> {
        let current = ConfigHandle::current();
        if config_db_path_matches_server(current.as_ref(), &self.db_path) {
            current
        } else {
            Arc::clone(&self.initial_config)
        }
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_async_db_for_test(mut self, async_db: AsyncDb) -> Self {
        let cell = Arc::new(OnceCell::new());
        debug_assert!(cell.set(async_db).is_ok());
        self.async_db = cell;
        self
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_async_queue_for_test(mut self, async_queue: AsyncPendingMessageStore) -> Self {
        self.async_queue = async_queue;
        self
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_ingest_processing_delay_for_test(mut self, delay: Duration) -> Self {
        self.ingest_processing_delay = Some(delay);
        self
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_mcp_deadline_for_test(mut self, deadline: Duration) -> Self {
        self.search_route_deadline = deadline;
        self.search_db_deadline = deadline;
        self.search_stale_index_deadline = deadline;
        self.ingest_admission_deadline = deadline;
        self.operation_status_deadline = deadline;
        self
    }

    pub async fn serve_stdio(
        self,
    ) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleServer, Self>> {
        crate::mcp::logging::init_stdio_log_sink(ConfigHandle::current().as_ref())
            .context("failed to initialize MCP log sink")?;
        self.gating_runtime
            .validate_config_shape()
            .context("failed to validate ingest gating config")?;
        let background = self.clone();
        let service = self
            .serve(rmcp::transport::stdio())
            .await
            .context("failed to initialize MCP stdio transport")?;
        background.spawn_stdio_background_tasks();
        Ok(service)
    }

    fn spawn_stdio_background_tasks(&self) {
        let reconcile = self.clone();
        let db_path = self.db_path.clone();
        tokio::spawn(async move {
            match tokio::task::spawn_blocking(move || reconcile.reconcile_reindex_progress()).await
            {
                Ok(Ok(0)) => {}
                Ok(Ok(updated)) => {
                    tracing::info!(
                        db_path = %db_path.display(),
                        updated,
                        "reconciled orphan reindex progress rows after MCP startup"
                    );
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        db_path = %db_path.display(),
                        error = %error,
                        "failed to reconcile orphan reindex progress rows after MCP startup"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        db_path = %db_path.display(),
                        error = %error,
                        "reindex progress reconciliation task failed after MCP startup"
                    );
                }
            }
        });
        self.spawn_ingest_drain_worker();
        let _gating_init_task =
            self.gating_runtime
                .spawn_initialize_from_config(Duration::from_secs(
                    ConfigHandle::current()
                        .as_ref()
                        .embed
                        .retry
                        .search_deadline_secs,
                ));
    }

    fn reconcile_reindex_progress(&self) -> anyhow::Result<usize> {
        let db = Database::open(&self.db_path)
            .context("failed to open database for reindex progress reconciliation")?;
        if db.vector_table_distance_metric()?.as_deref() != Some(VECTOR_DISTANCE_METRIC) {
            return Ok(0);
        }
        let Some(dim) = Self::current_vector_dim(&db)? else {
            return Ok(0);
        };
        let target_fingerprint = Database::current_vector_embedder_fingerprint(dim);
        ReindexProgressStore::new(&self.db_path)
            .finalize_completed_running_rows(CURRENT_VECTOR_INDEX_VERSION, &target_fingerprint)
            .context("failed to finalize orphan reindex progress rows")
    }

    fn current_vector_dim(db: &Database) -> anyhow::Result<Option<usize>> {
        let dim = db
            .conn()
            .query_row(
                "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("failed to read current vector dimension")?
            .map(|value| value as usize);
        Ok(dim)
    }

    fn spawn_ingest_drain_worker(&self) {
        if self.ingest_worker_started.swap(true, Ordering::AcqRel) {
            return;
        }

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.ingest_worker_started.store(false, Ordering::Release);
            return;
        };

        let worker = self.clone();
        handle.spawn(async move {
            let _started_guard =
                IngestDrainWorkerStartedGuard::new(Arc::clone(&worker.ingest_worker_started));
            worker.supervise_ingest_drain_worker().await;
        });
    }

    async fn supervise_ingest_drain_worker(self) {
        let mut restart_backoff_ms = INGEST_DRAIN_RESTART_BACKOFF_INITIAL_MS;
        loop {
            let worker = self.clone();
            let join_handle = tokio::spawn(async move {
                worker.run_ingest_drain_worker().await;
            });

            match join_handle.await {
                Ok(()) => {
                    tracing::error!(
                        db_path = %self.db_path.display(),
                        "async ingest worker exited unexpectedly; restarting"
                    );
                }
                Err(error) => {
                    tracing::error!(
                        db_path = %self.db_path.display(),
                        error = %error,
                        "async ingest worker stopped unexpectedly; restarting"
                    );
                }
            }

            tokio::time::sleep(Duration::from_millis(restart_backoff_ms)).await;
            restart_backoff_ms = restart_backoff_ms
                .saturating_mul(2)
                .min(INGEST_DRAIN_RESTART_BACKOFF_MAX_MS);
        }
    }

    async fn run_ingest_drain_worker(self) {
        let worker_id = format!(
            "mcp-ingest-worker-{:x}-{:x}",
            std::process::id(),
            Arc::as_ptr(&self.ingest_worker_started) as usize
        );
        let queue = self.async_queue.clone();

        loop {
            match queue
                .claim_next_by_kind(
                    worker_id.clone(),
                    INGEST_CLAIM_TTL_SECS,
                    INGEST_ASYNC_KIND.to_string(),
                )
                .await
            {
                Ok(Some(claim)) => {
                    if let Err(error) = self.process_ingest_claim(&queue, &worker_id, claim).await {
                        tracing::warn!(error = %error, "async ingest worker failed to process op");
                    }
                }
                Ok(None) => tokio::time::sleep(INGEST_POLL_INTERVAL).await,
                Err(error) => {
                    tracing::warn!(error = %error, "async ingest worker claim failed");
                    tokio::time::sleep(INGEST_POLL_INTERVAL).await;
                }
            }
        }
    }

    async fn process_ingest_claim(
        &self,
        queue: &AsyncPendingMessageStore,
        worker_id: &str,
        claim: ClaimedMessage,
    ) -> anyhow::Result<()> {
        let queue_wait_ms = queue_wait_ms(claim.created_at, claim.claimed_at);
        let prepared: PreparedIngestOperation = match serde_json::from_str(&claim.payload) {
            Ok(prepared) => prepared,
            Err(error) => {
                let detail = format!("failed to decode ingest operation {}: {error}", claim.id);
                complete_failed_ingest_claim(queue, &claim, queue_wait_ms, detail).await?;
                return Ok(());
            }
        };

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let heartbeat_queue = queue.clone();
        let heartbeat_id = claim.id.clone();
        let heartbeat_worker_id = worker_id.to_string();
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(INGEST_HEARTBEAT_INTERVAL);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        if let Err(error) = heartbeat_queue.refresh_heartbeat(heartbeat_id.clone(), heartbeat_worker_id.clone()).await {
                            tracing::warn!(error = %error, claim_id = %heartbeat_id, "failed to refresh async ingest heartbeat");
                            break;
                        }
                    }
                    _ = &mut stop_rx => break,
                }
            }
        });

        let outcome = match self
            .run_prepared_ingest_off_runtime(prepared.request.clone(), prepared.controls)
            .await
        {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(error.to_string()),
            Err(error) => Err(error.to_string()),
        };

        let _ = stop_tx.send(());
        let _ = heartbeat.await;

        match outcome {
            Ok(mut response) => {
                let rejected_reason = response
                    .gating_decision
                    .as_ref()
                    .and_then(|decision| decision.drop_reason().map(ToOwned::to_owned));
                let state = if response.dropped && rejected_reason.is_some() {
                    IngestOperationState::Rejected
                } else {
                    IngestOperationState::Completed
                };
                if matches!(state, IngestOperationState::Rejected) {
                    response.drawer_id.clear();
                    response.drawer_ids.clear();
                }
                let mut finalized = finalize_ingest_response(
                    claim.id.clone(),
                    claim.created_at,
                    response,
                    state,
                    rejected_reason.clone(),
                    None,
                );
                finalized
                    .timings
                    .insert("queue_wait_ms".to_string(), queue_wait_ms);
                let result_json = serde_json::to_string(&finalized)
                    .context("failed to serialize completed ingest response")?;
                let result_drawer_id = match state {
                    IngestOperationState::Completed if !finalized.drawer_id.is_empty() => {
                        Some(finalized.drawer_id.as_str())
                    }
                    _ => None,
                };
                queue
                    .complete_operation(
                        claim.clone(),
                        state.as_str().to_string(),
                        result_drawer_id.map(ToOwned::to_owned),
                        rejected_reason.clone(),
                        None,
                        Some(result_json),
                    )
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("failed to store async ingest result: {error}")
                    })?;
            }
            Err(detail) => {
                complete_failed_ingest_claim(queue, &claim, queue_wait_ms, detail).await?;
            }
        }

        Ok(())
    }

    async fn run_prepared_ingest_off_runtime(
        &self,
        request: IngestRequest,
        controls: IngestControls,
    ) -> anyhow::Result<std::result::Result<IngestResponse, ErrorData>> {
        let worker = self.clone();
        #[cfg(any(test, feature = "db-test-seam"))]
        let ingest_processing_delay = self.ingest_processing_delay;
        let dispatcher = tracing::dispatcher::get_default(Clone::clone);
        tokio::task::spawn_blocking(move || {
            tracing::dispatcher::with_default(&dispatcher, || {
                #[cfg(any(test, feature = "db-test-seam"))]
                if let Some(delay) = ingest_processing_delay {
                    std::thread::sleep(delay);
                }
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("failed to build blocking ingest runtime")?;
                Ok(runtime
                    .block_on(worker.mempal_ingest_sync(request, controls))
                    .map(|response| response.0))
            })
        })
        .await
        .context("blocking ingest worker task failed")?
    }

    pub(super) fn open_db(&self) -> std::result::Result<Database, ErrorData> {
        Database::open(&self.db_path).map_err(|error| {
            ErrorData::internal_error(format!("failed to open database: {error}"), None)
        })
    }

    async fn async_db(&self) -> anyhow::Result<AsyncDb> {
        let db_path = self.db_path.clone();
        let async_db = self
            .async_db
            .get_or_try_init(|| async move {
                let display_path = db_path.display().to_string();
                tokio::task::spawn_blocking(move || {
                    AsyncDb::open(&db_path, 4).with_context(|| {
                        format!("failed to open MCP async database pool for {display_path}")
                    })
                })
                .await
                .context("blocking MCP async database pool open failed")?
            })
            .await?;
        Ok(async_db.clone())
    }

    async fn load_status_db_snapshot(
        &self,
        project_scope: ProjectSearchScope,
        turns_config: crate::core::config::TurnsConfig,
    ) -> anyhow::Result<StatusDbSnapshot> {
        let async_db = self.async_db().await?;
        async_db
            .run_read_anyhow(move |db| {
                let schema_version = db.schema_version()?;
                let fork_ext_version = read_fork_ext_version(db.conn())?;
                let stale_drawer_count = db.stale_drawer_count(CURRENT_NORMALIZE_VERSION)? as u64;
                let vector_index_stale = db.vector_index_is_stale().unwrap_or(false);
                let drawer_count = db.drawer_count()?;
                let vector_rows = db.vector_row_count()?;
                let vector_index_empty = vector_rows == 0 && drawer_count > 0;
                let consolidation_stats = db.consolidation_stats()?;
                let pending_card_count = db.pending_auto_generated_knowledge_card_count()?;
                let last_crystallization_at = db.last_crystallization_at()?;
                let design_insight_summary =
                    crate::core::design_insights::unresolved_design_insight_summary(db.conn())?;
                let raw_turn_count = count_raw_turn_drawers(db, &turns_config)?;
                let null_project_backfill_pending = db.null_project_backfill_pending_count()?;
                let taxonomy_count = db.taxonomy_count()?;
                let db_size_bytes = db.database_size_bytes()?;
                let diary_rollup_days = db.diary_rollup_days()?;
                let scopes = db
                    .scope_counts_for_search_scope(&project_scope)?
                    .into_iter()
                    .map(|(wing, room, drawer_count)| ScopeCount {
                        wing,
                        room,
                        drawer_count,
                    })
                    .collect();
                let source_type_distribution = db
                    .source_type_counts()?
                    .into_iter()
                    .map(|(source_type, count)| SourceTypeCount {
                        source_type: source_type.to_string(),
                        count,
                    })
                    .collect();
                let pinned_fact_counts = db
                    .pinned_fact_counts_by_project()?
                    .into_iter()
                    .map(|(project_id, count)| PinnedFactProjectCount { project_id, count })
                    .collect();
                Ok(StatusDbSnapshot {
                    schema_version,
                    fork_ext_version,
                    stale_drawer_count,
                    vector_index_stale,
                    drawer_count,
                    vector_rows,
                    vector_index_empty,
                    total_compacted_drawers: consolidation_stats.total_compacted_drawers,
                    consolidation_runs: consolidation_stats.consolidation_runs,
                    last_consolidation_at: consolidation_stats.last_consolidation_at,
                    last_sleep_at: consolidation_stats.last_sleep_at,
                    sleep_items_pruned: consolidation_stats.sleep_items_pruned,
                    sleep_items_compacted: consolidation_stats.sleep_items_compacted,
                    sleep_conflicts_resolved: consolidation_stats.sleep_conflicts_resolved,
                    pending_card_count,
                    last_crystallization_at,
                    design_insight_summary,
                    raw_turn_count,
                    null_project_backfill_pending,
                    taxonomy_count,
                    db_size_bytes,
                    diary_rollup_days,
                    scopes,
                    source_type_distribution,
                    pinned_fact_counts,
                })
            })
            .await
            .context("status database snapshot failed")
    }

    pub(super) async fn resolve_mcp_project_id(
        &self,
        explicit: Option<&str>,
        config: &crate::core::config::Config,
    ) -> std::result::Result<Option<String>, ErrorData> {
        if let Some(explicit) = explicit {
            return validate_project_id(explicit).map(Some).map_err(|error| {
                ErrorData::invalid_params(format!("invalid project scope: {error}"), None)
            });
        }

        if let Some(configured) = config.project.id.as_deref() {
            return validate_project_id(configured).map(Some).map_err(|error| {
                ErrorData::invalid_params(format!("invalid project scope: {error}"), None)
            });
        }

        if let Ok(guard) = self.client_project_id.lock()
            && let Some(project_id) = guard.clone()
        {
            return Ok(Some(project_id));
        }

        let peer = self.client_peer.lock().ok().and_then(|guard| guard.clone());
        let client_supports_roots = peer
            .as_ref()
            .and_then(|p| p.peer_info())
            .and_then(|info| info.capabilities.roots.clone())
            .is_some();
        if let Some(peer) = peer
            && client_supports_roots
            && let Ok(result) = peer.list_roots().await
            && let Some(project_id) = result
                .roots
                .into_iter()
                .find_map(|root| infer_project_id_from_root_uri(&root.uri).ok().flatten())
        {
            if let Ok(mut guard) = self.client_project_id.lock() {
                *guard = Some(project_id.clone());
            }
            return Ok(Some(project_id));
        }

        Ok(None)
    }

    pub async fn ingest_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<IngestResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let response = self
            .mempal_ingest(Parameters(request))
            .await
            .map(|response| response.0)?;
        match response.operation_id.as_deref() {
            Some(operation_id) => self.wait_for_operation_completion(operation_id).await,
            None => Ok(response),
        }
    }

    pub async fn operation_status_json_for_test(
        &self,
        operation_id: &str,
    ) -> std::result::Result<IngestResponse, ErrorData> {
        self.mempal_operation_status(Parameters(OperationStatusRequest {
            operation_id: operation_id.to_string(),
        }))
        .await
        .map(|response| response.0)
    }

    pub async fn wait_for_operation_status(
        &self,
        operation_id: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> std::result::Result<Option<IngestResponse>, ErrorData> {
        let deadline = Instant::now() + clamp_wait_timeout(timeout);
        loop {
            let response = self.operation_status_json_for_test(operation_id).await?;
            if response
                .state
                .map(IngestOperationState::is_terminal)
                .unwrap_or(false)
            {
                return Ok(Some(response));
            }
            if timeout.is_zero() || Instant::now() >= deadline {
                return Ok(None);
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    pub async fn wait_for_operation_completion(
        &self,
        operation_id: &str,
    ) -> std::result::Result<IngestResponse, ErrorData> {
        match self
            .wait_for_operation_status(
                operation_id,
                Duration::from_secs(30),
                Duration::from_millis(150),
            )
            .await?
        {
            Some(response) => Ok(response),
            None => Err(ErrorData::internal_error(
                format!("timed out waiting for ingest operation {operation_id}"),
                None,
            )),
        }
    }

    async fn run_read_anyhow_bounded<F, R>(
        &self,
        f: F,
        deadline: Duration,
    ) -> anyhow::Result<Option<R>>
    where
        F: FnOnce(&Database) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let async_db = self.async_db().await?;
        match tokio::time::timeout(deadline, async_db.run_read_anyhow(f)).await {
            Ok(result) => result.map(Some),
            Err(_) => Ok(None),
        }
    }

    async fn run_bm25_search_bounded(
        &self,
        query: String,
        route: crate::core::types::RouteDecision,
        scope: ProjectSearchScope,
        search_options: SearchOptions,
        top_k: usize,
    ) -> anyhow::Result<Option<Vec<SearchResult>>> {
        self.run_read_anyhow_bounded(
            move |db| {
                search_bm25_only_with_options(db, &query, route, &scope, search_options, top_k)
                    .map_err(|error| anyhow::anyhow!("BM25 search failed: {error}"))
            },
            self.search_db_deadline,
        )
        .await
    }

    fn handle_search_database_error<R>(
        &self,
        error: anyhow::Error,
        stage: &str,
        response_warnings: &mut Vec<String>,
        system_warnings: &mut Vec<SystemWarning>,
    ) -> std::result::Result<Option<R>, ErrorData> {
        if push_mcp_search_database_warning(
            response_warnings,
            system_warnings,
            &self.db_path,
            stage,
            error.as_ref(),
        ) {
            return Ok(None);
        }

        Err(ErrorData::internal_error(error.to_string(), None))
    }

    async fn system_warnings_with_stale_index_bounded(
        &self,
        deadline: Duration,
    ) -> std::result::Result<Vec<SystemWarning>, ErrorData> {
        match self
            .run_read_anyhow_bounded(|db| Ok(system_warnings_with_stale_index(db)), deadline)
            .await
            .map_err(db_error)?
        {
            Some(warnings) => Ok(warnings),
            None => {
                let mut warnings = current_system_warnings();
                warnings.push(SystemWarning {
                    level: "warn".to_string(),
                    message: mcp_stage_timeout_warning("stale vector index check", deadline),
                    source: "mcp_timeout".to_string(),
                });
                Ok(warnings)
            }
        }
    }

    async fn ingest_system_warnings_with_stale_index_bounded(
        &self,
        deadline: Duration,
    ) -> std::result::Result<Vec<SystemWarning>, ErrorData> {
        match self
            .run_read_anyhow_bounded(|db| Ok(system_warnings_with_stale_index(db)), deadline)
            .await
        {
            Ok(Some(warnings)) => Ok(warnings),
            Ok(None) => {
                let mut warnings = current_system_warnings();
                warnings.push(SystemWarning {
                    level: "warn".to_string(),
                    message: mcp_stage_timeout_warning("stale vector index check", deadline),
                    source: "mcp_timeout".to_string(),
                });
                Ok(warnings)
            }
            Err(error) => Err(database_write_refused_error(
                &self.db_path,
                "stale vector index check",
                error.as_ref(),
            )),
        }
    }

    pub async fn search_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<SearchResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_search(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn context_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<ContextResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_context(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn knowledge_gate_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<KnowledgeGateResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_knowledge_gate(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn knowledge_distill_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<KnowledgeDistillResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_knowledge_distill(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn knowledge_promote_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<KnowledgePromoteResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_knowledge_promote(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn knowledge_demote_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<KnowledgeDemoteResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_knowledge_demote(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn knowledge_publish_anchor_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<KnowledgePublishAnchorResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_knowledge_publish_anchor(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn tunnels_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<TunnelsResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_tunnels(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn status_json_for_test(&self) -> std::result::Result<StatusResponse, ErrorData> {
        self.mempal_status().await.map(|response| response.0)
    }

    pub async fn status_json_with_request_for_test(
        &self,
        request: StatusRequest,
    ) -> std::result::Result<StatusResponse, ErrorData> {
        self.mempal_status_with_options(request)
            .await
            .map(|response| response.0)
    }

    pub async fn mempal_status(&self) -> std::result::Result<Json<StatusResponse>, ErrorData> {
        self.mempal_status_with_options(StatusRequest::default())
            .await
    }

    pub async fn mempal_status_with_options(
        &self,
        request: StatusRequest,
    ) -> std::result::Result<Json<StatusResponse>, ErrorData> {
        self.mempal_status_tool(Parameters(request)).await
    }

    pub async fn pinned_facts_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<PinnedFactsResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_pinned_facts(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn knowledge_policy_json_for_test(
        &self,
    ) -> std::result::Result<KnowledgePolicyResponse, ErrorData> {
        self.mempal_knowledge_policy()
            .await
            .map(|response| response.0)
    }

    pub async fn knowledge_cards_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<KnowledgeCardsResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_knowledge_cards(Parameters(request))
            .await
            .map(|response| response.0)
    }

    pub async fn field_taxonomy_json_for_test(
        &self,
    ) -> std::result::Result<FieldTaxonomyResponse, ErrorData> {
        self.mempal_field_taxonomy()
            .await
            .map(|response| response.0)
    }

    pub async fn phase3_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<Phase3Response, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_phase3(Parameters(request))
            .await
            .map(|response| response.0)
    }

    /// Fallback helper for novelty merge paths that fall back to insert
    /// (e.g. merge cap reached, re-embed failed). Inserts all chunks as
    /// separate drawers, mirroring the Insert branch.
    #[allow(clippy::too_many_arguments)]
    fn mcp_ingest_insert_fallback(
        &self,
        db: &mut Database,
        primary_drawer_id: &str,
        _scrubbed_content: &str,
        request: &IngestRequest,
        chunks: &[String],
        vectors: &[Vec<f32>],
        chunk_drawer_ids: &[(usize, String, bool)],
        mempal_home: &std::path::Path,
        project_id: Option<&str>,
        near_target_id: &str,
        novelty: &crate::ingest::novelty::NoveltyDecision,
        audit_decision: Option<&str>,
        importance: i32,
        source_type: SourceType,
        confidence: f64,
        inserted_drawer_ids: &mut Vec<String>,
        newly_created_drawer_ids: &mut Vec<String>,
    ) -> std::result::Result<(), ErrorData> {
        let metadata = validate_ingest_request(request, &source_type)?;
        db.record_novelty_audit(
            primary_drawer_id,
            NoveltyAction::Insert,
            Some(near_target_id),
            novelty.cosine,
            audit_decision,
            project_id,
        )
        .map_err(db_error)?;

        for ((chunk_idx, chunk_did, _), (chunk, vector)) in chunk_drawer_ids
            .iter()
            .zip(chunks.iter().zip(vectors.iter()))
        {
            let _extra_lock = if *chunk_idx > 0 {
                Some(
                    crate::ingest::lock::acquire_source_lock(
                        mempal_home,
                        chunk_did,
                        std::time::Duration::from_secs(5),
                    )
                    .map_err(|e| {
                        ErrorData::internal_error(
                            format!("ingest lock chunk {chunk_idx}: {e}"),
                            None,
                        )
                    })?,
                )
            } else {
                None
            };
            let exists = db.drawer_exists(chunk_did).map_err(db_error)?;
            if exists {
                // Dedup-resolved: drawer pre-existed; include in response list but NOT
                // in newly_created_drawer_ids so LLM reject cannot soft-delete it.
                if metadata.is_pinned {
                    db.pin_drawer(chunk_did, None).map_err(db_error)?;
                }
                inserted_drawer_ids.push(chunk_did.clone());
                continue;
            }
            let drawer = drawer_from_ingest_metadata(
                request,
                &metadata,
                chunk_did,
                chunk,
                *chunk_idx,
                SourceConfidence {
                    source_type,
                    confidence,
                },
                importance,
            );
            db.insert_drawer_with_project_validity(
                &drawer,
                project_id,
                None,
                request.valid_from.as_deref(),
                request.valid_until.as_deref(),
            )
            .map_err(db_error)?;
            db.insert_vector_with_project(chunk_did, vector, project_id)
                .map_err(db_error)?;
            inserted_drawer_ids.push(chunk_did.clone());
            newly_created_drawer_ids.push(chunk_did.clone());
        }
        Ok(())
    }
}

// =========================================================================
// Knowledge-system ingest validation (upstream)
// These helpers are part of the upstream knowledge lifecycle API surface.
// The fork's ingest handler uses a different code path (gating + novelty),
// but these are retained for the knowledge distill/gate/promote tools.
// =========================================================================

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidatedIngestMetadata {
    memory_kind: MemoryKind,
    domain: MemoryDomain,
    field: String,
    is_pinned: bool,
    anchor_kind: AnchorKind,
    anchor_id: String,
    parent_anchor_id: Option<String>,
    provenance: Option<Provenance>,
    statement: Option<String>,
    tier: Option<KnowledgeTier>,
    status: Option<KnowledgeStatus>,
    supporting_refs: Vec<String>,
    counterexample_refs: Vec<String>,
    teaching_refs: Vec<String>,
    verification_refs: Vec<String>,
    scope_constraints: Option<String>,
    trigger_hints: Option<TriggerHints>,
}

#[allow(dead_code)]
impl ValidatedIngestMetadata {
    fn identity_parts(&self) -> BootstrapIdentityParts<'_> {
        BootstrapIdentityParts {
            memory_kind: &self.memory_kind,
            domain: &self.domain,
            field: &self.field,
            anchor_kind: &self.anchor_kind,
            anchor_id: &self.anchor_id,
            parent_anchor_id: self.parent_anchor_id.as_deref(),
            provenance: self.provenance.as_ref(),
            statement: self.statement.as_deref(),
            tier: self.tier.as_ref(),
            status: self.status.as_ref(),
            supporting_refs: &self.supporting_refs,
            counterexample_refs: &self.counterexample_refs,
            teaching_refs: &self.teaching_refs,
            verification_refs: &self.verification_refs,
            scope_constraints: self.scope_constraints.as_deref(),
            trigger_hints: self.trigger_hints.as_ref(),
        }
    }
}

const MAX_WAIT_TIMEOUT_SECS: u64 = 86_400;

fn clamp_wait_timeout(timeout: Duration) -> Duration {
    timeout.min(Duration::from_secs(MAX_WAIT_TIMEOUT_SECS))
}

#[allow(dead_code)]
fn validate_ingest_request(
    request: &IngestRequest,
    source_type: &SourceType,
) -> std::result::Result<ValidatedIngestMetadata, ErrorData> {
    validate_temporal_param("valid_from", request.valid_from.as_deref())?;
    validate_temporal_param("valid_until", request.valid_until.as_deref())?;

    let memory_kind =
        parse_memory_kind(request.memory_kind.as_deref())?.unwrap_or(MemoryKind::Evidence);
    let domain = parse_domain(request.domain.as_deref())?.unwrap_or(MemoryDomain::Project);
    let field = trim_to_option(request.field.as_deref())
        .unwrap_or(anchor::DEFAULT_FIELD)
        .to_string();
    let statement = trim_to_owned(request.statement.as_deref());
    let tier = parse_tier(request.tier.as_deref())?;
    let status = parse_status(request.status.as_deref())?;
    let provenance = parse_provenance(request.provenance.as_deref())?;
    let supporting_refs = normalize_refs(request.supporting_refs.as_deref());
    let counterexample_refs = normalize_refs(request.counterexample_refs.as_deref());
    let teaching_refs = normalize_refs(request.teaching_refs.as_deref());
    let verification_refs = normalize_refs(request.verification_refs.as_deref());
    let scope_constraints = trim_to_owned(request.scope_constraints.as_deref());
    let trigger_hints = request.trigger_hints.as_ref().map(trigger_hints_from_dto);

    let derived_anchor = validate_anchor_metadata(request, &domain, source_type)?;

    match memory_kind {
        MemoryKind::Evidence => {
            if statement.is_some()
                || tier.is_some()
                || !supporting_refs.is_empty()
                || !counterexample_refs.is_empty()
                || !teaching_refs.is_empty()
                || !verification_refs.is_empty()
                || scope_constraints.is_some()
                || trigger_hints.is_some()
            {
                return Err(ErrorData::invalid_params(
                    "evidence drawer does not allow knowledge-only fields",
                    None,
                ));
            }
            if status.as_ref().is_some_and(|value| {
                !matches!(value, KnowledgeStatus::Active | KnowledgeStatus::Canonical)
            }) {
                return Err(ErrorData::invalid_params(
                    "evidence status must be active or canonical",
                    None,
                ));
            }

            Ok(ValidatedIngestMetadata {
                memory_kind,
                domain,
                field,
                is_pinned: request.is_pinned.unwrap_or(false),
                anchor_kind: derived_anchor.anchor_kind,
                anchor_id: derived_anchor.anchor_id,
                parent_anchor_id: derived_anchor.parent_anchor_id,
                provenance: Some(
                    provenance.unwrap_or_else(|| anchor::bootstrap_provenance(source_type)),
                ),
                statement: None,
                tier: None,
                status,
                supporting_refs: Vec::new(),
                counterexample_refs: Vec::new(),
                teaching_refs: Vec::new(),
                verification_refs: Vec::new(),
                scope_constraints: None,
                trigger_hints: None,
            })
        }
        MemoryKind::AtomicFact
        | MemoryKind::Decision
        | MemoryKind::Case
        | MemoryKind::Skill
        | MemoryKind::Foresight
        | MemoryKind::ProfileFact
        | MemoryKind::ProfileTrait => {
            if tier.is_some()
                || !supporting_refs.is_empty()
                || !counterexample_refs.is_empty()
                || !teaching_refs.is_empty()
                || !verification_refs.is_empty()
            {
                return Err(ErrorData::invalid_params(
                    "typed record drawer does not allow knowledge-only tier/ref fields",
                    None,
                ));
            }
            if status.as_ref().is_some_and(|value| {
                !matches!(value, KnowledgeStatus::Active | KnowledgeStatus::Canonical)
            }) {
                return Err(ErrorData::invalid_params(
                    "typed record status must be active or canonical",
                    None,
                ));
            }

            Ok(ValidatedIngestMetadata {
                memory_kind,
                domain,
                field,
                is_pinned: request.is_pinned.unwrap_or(false),
                anchor_kind: derived_anchor.anchor_kind,
                anchor_id: derived_anchor.anchor_id,
                parent_anchor_id: derived_anchor.parent_anchor_id,
                provenance: Some(
                    provenance.unwrap_or_else(|| anchor::bootstrap_provenance(source_type)),
                ),
                statement,
                tier: None,
                status,
                supporting_refs: Vec::new(),
                counterexample_refs: Vec::new(),
                teaching_refs: Vec::new(),
                verification_refs: Vec::new(),
                scope_constraints,
                trigger_hints,
            })
        }
        MemoryKind::Knowledge => {
            if provenance.is_some() {
                return Err(ErrorData::invalid_params(
                    "knowledge drawer does not allow provenance",
                    None,
                ));
            }

            let statement = statement.ok_or_else(|| {
                ErrorData::invalid_params(
                    "knowledge drawer requires statement and supporting_refs",
                    None,
                )
            })?;
            let tier = tier.ok_or_else(|| {
                ErrorData::invalid_params(
                    "knowledge drawer requires tier, status, statement, and supporting_refs",
                    None,
                )
            })?;
            let status = status.ok_or_else(|| {
                ErrorData::invalid_params(
                    "knowledge drawer requires tier, status, statement, and supporting_refs",
                    None,
                )
            })?;

            if supporting_refs.is_empty() {
                return Err(ErrorData::invalid_params(
                    "knowledge drawer requires statement and supporting_refs",
                    None,
                ));
            }
            validate_drawer_refs("supporting_refs", &supporting_refs)?;
            validate_drawer_refs("counterexample_refs", &counterexample_refs)?;
            validate_drawer_refs("teaching_refs", &teaching_refs)?;
            validate_drawer_refs("verification_refs", &verification_refs)?;

            validate_tier_status(&tier, &status)?;

            Ok(ValidatedIngestMetadata {
                memory_kind,
                domain,
                field,
                is_pinned: request.is_pinned.unwrap_or(false),
                anchor_kind: derived_anchor.anchor_kind,
                anchor_id: derived_anchor.anchor_id,
                parent_anchor_id: derived_anchor.parent_anchor_id,
                provenance: None,
                statement: Some(statement),
                tier: Some(tier),
                status: Some(status),
                supporting_refs,
                counterexample_refs,
                teaching_refs,
                verification_refs,
                scope_constraints,
                trigger_hints,
            })
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PreparedIngestOperation {
    request: IngestRequest,
    #[serde(default)]
    controls: IngestControls,
    project_id: Option<String>,
    scrubbed_content: String,
    source_type: SourceType,
    confidence: f64,
    metadata: ValidatedIngestMetadata,
    superseded_drawer_id: Option<String>,
    raw_turn: bool,
    drawer_importance: i32,
}

const INGEST_ASYNC_KIND: &str = "ingest_async";
const INGEST_CLAIM_TTL_SECS: i64 = 300;
const INGEST_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INGEST_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const INGEST_DRAIN_RESTART_BACKOFF_INITIAL_MS: u64 = 250;
const INGEST_DRAIN_RESTART_BACKOFF_MAX_MS: u64 = 5_000;

struct IngestDrainWorkerStartedGuard {
    started: Arc<AtomicBool>,
}

impl IngestDrainWorkerStartedGuard {
    fn new(started: Arc<AtomicBool>) -> Self {
        Self { started }
    }
}

impl Drop for IngestDrainWorkerStartedGuard {
    fn drop(&mut self) {
        self.started.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct StatusDbSnapshot {
    schema_version: u32,
    fork_ext_version: u32,
    stale_drawer_count: u64,
    vector_index_stale: bool,
    drawer_count: i64,
    vector_rows: i64,
    vector_index_empty: bool,
    total_compacted_drawers: u64,
    consolidation_runs: u64,
    last_consolidation_at: Option<String>,
    last_sleep_at: Option<String>,
    sleep_items_pruned: u64,
    sleep_items_compacted: u64,
    sleep_conflicts_resolved: u64,
    pending_card_count: i64,
    last_crystallization_at: Option<String>,
    design_insight_summary: crate::core::design_insights::DesignInsightSummary,
    raw_turn_count: i64,
    null_project_backfill_pending: i64,
    taxonomy_count: i64,
    db_size_bytes: u64,
    diary_rollup_days: u32,
    scopes: Vec<ScopeCount>,
    source_type_distribution: Vec<SourceTypeCount>,
    pinned_fact_counts: Vec<PinnedFactProjectCount>,
}

fn default_queue_stats() -> crate::core::queue::QueueStats {
    crate::core::queue::QueueStats {
        pending: 0,
        claimed: 0,
        failed: 0,
        failed_retryable: 0,
        failed_terminal: 0,
        failed_retryable_embed: 0,
        failed_retryable_llm: 0,
        last_auto_requeue_at_unix_ms: None,
        oldest_pending_age_secs: None,
        rate_per_min: 0.0,
        avg_processing_ms: None,
        eta_secs: None,
    }
}

fn status_error_summary(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(error) = source {
        parts.push(error.to_string());
        source = error.source();
    }
    parts.dedup();
    parts.join(": ")
}

fn status_db_failure_kind(error: &(dyn std::error::Error + 'static)) -> &'static str {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(sqlite) = error.downcast_ref::<rusqlite::Error>()
            && let Some(kind) = status_rusqlite_failure_kind(sqlite)
        {
            return kind;
        }
        current = error.source();
    }

    let summary = status_error_summary(error).to_ascii_lowercase();
    if summary.contains("database is locked")
        || summary.contains("database locked")
        || summary.contains("database is busy")
        || summary.contains("database busy")
        || summary.contains("sqlite_busy")
        || summary.contains("sqlite_locked")
    {
        "locked_or_busy"
    } else if summary.contains("permission denied")
        || summary.contains("readonly")
        || summary.contains("read-only")
        || summary.contains("unable to open database file")
        || summary.contains("no such file")
        || summary.contains("not found")
        || summary.contains("cannot open")
    {
        "path_or_permission"
    } else if summary.contains("not a database")
        || summary.contains("database disk image is malformed")
        || summary.contains("malformed")
        || summary.contains("corrupt")
    {
        "corrupt_or_invalid"
    } else {
        "unknown"
    }
}

fn status_rusqlite_failure_kind(error: &rusqlite::Error) -> Option<&'static str> {
    match error {
        rusqlite::Error::SqliteFailure(sqlite, _) => match sqlite.code {
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked => {
                Some("locked_or_busy")
            }
            rusqlite::ErrorCode::CannotOpen
            | rusqlite::ErrorCode::NotFound
            | rusqlite::ErrorCode::PermissionDenied
            | rusqlite::ErrorCode::ReadOnly => Some("path_or_permission"),
            rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase => {
                Some("corrupt_or_invalid")
            }
            _ => None,
        },
        _ => None,
    }
}

fn status_db_failure_hint(kind: &str) -> &'static str {
    match kind {
        "locked_or_busy" => {
            "Check for stale daemon/MCP processes holding palace.db, wait for the writer to finish, then retry status."
        }
        "path_or_permission" => {
            "Check that the configured database path exists, is a SQLite file, and is readable/writable by the current user."
        }
        "corrupt_or_invalid" => {
            "Back up the database files, then inspect or restore palace.db before running repair or init commands."
        }
        _ => {
            "Inspect daemon logs, database holders, file permissions, and SQLite integrity before retrying."
        }
    }
}

fn status_database_diagnostic(
    db_path: &Path,
    source: &str,
    error: &(dyn std::error::Error + 'static),
) -> DatabaseDiagnosticDto {
    let failure_kind = status_db_failure_kind(error);
    DatabaseDiagnosticDto {
        path: db_path.display().to_string(),
        source: source.to_string(),
        failure_kind: failure_kind.to_string(),
        summary: status_error_summary(error),
        hint: status_db_failure_hint(failure_kind).to_string(),
    }
}

fn record_status_database_diagnostic(
    system_warnings: &mut Vec<SystemWarning>,
    database_diagnostic: &mut Option<DatabaseDiagnosticDto>,
    diagnostic: DatabaseDiagnosticDto,
) {
    system_warnings.push(SystemWarning {
        level: "warn".to_string(),
        message: format!(
            "database diagnostic degraded at {}: {} ({})",
            diagnostic.source, diagnostic.summary, diagnostic.failure_kind
        ),
        source: "database".to_string(),
    });
    if database_diagnostic.is_none() {
        *database_diagnostic = Some(diagnostic);
    }
}

fn validate_temporal_param(name: &str, value: Option<&str>) -> std::result::Result<(), ErrorData> {
    if let Some(raw) = value
        && crate::core::decay::parse_temporal_timestamp_secs(raw).is_none()
    {
        return Err(ErrorData::invalid_params(
            format!("{name} must be a Unix timestamp or RFC3339 timestamp"),
            None,
        ));
    }
    Ok(())
}

fn parse_source_type_param(value: Option<&str>) -> std::result::Result<SourceType, ErrorData> {
    match value {
        Some(raw) => raw.parse::<SourceType>().map_err(|_| {
            ErrorData::invalid_params(
                "source_type must be one of: user_explicit, agent_observation, agent_inference, system_generated",
                None,
            )
        }),
        None => Ok(SourceType::AgentInference),
    }
}

fn should_apply_async_llm_gating(source_type: SourceType) -> bool {
    matches!(
        source_type,
        SourceType::SystemGenerated | SourceType::AgentObservation
    )
}

fn resolve_confidence_param(
    source_type: SourceType,
    value: Option<f64>,
) -> std::result::Result<f64, ErrorData> {
    match value {
        Some(confidence) if confidence.is_finite() && (0.0..=1.0).contains(&confidence) => {
            Ok(confidence)
        }
        Some(_) => Err(ErrorData::invalid_params(
            "confidence must be a finite float between 0.0 and 1.0",
            None,
        )),
        None => Ok(default_confidence(source_type)),
    }
}

#[derive(Clone, Copy)]
struct SourceConfidence {
    source_type: SourceType,
    confidence: f64,
}

fn format_pinned_facts_text(drawers: &[Drawer]) -> String {
    if drawers.is_empty() {
        return "Pinned facts: none".to_string();
    }

    let mut lines = vec!["Pinned facts:".to_string()];
    for drawer in drawers {
        let source = drawer.source_file.as_deref().unwrap_or(drawer.id.as_str());
        let field = drawer.field.as_str();
        lines.push(format!(
            "- [{}] {}/{field} source={} importance={}: {}",
            drawer.id,
            match &drawer.domain {
                MemoryDomain::Project => "project",
                MemoryDomain::User => "user",
                MemoryDomain::Agent => "agent",
                MemoryDomain::Skill => "skill",
                MemoryDomain::Global => "global",
            },
            source,
            drawer.importance,
            drawer.content
        ));
    }
    lines.join("\n")
}

fn drawer_from_ingest_metadata(
    request: &IngestRequest,
    metadata: &ValidatedIngestMetadata,
    drawer_id: &str,
    content: &str,
    chunk_idx: usize,
    source_confidence: SourceConfidence,
    importance: i32,
) -> Drawer {
    let source_file = if metadata.memory_kind.is_knowledge() {
        Some(knowledge_source_file(
            &metadata.domain,
            &metadata.field,
            metadata.tier.as_ref().expect("validated knowledge tier"),
            metadata
                .statement
                .as_deref()
                .expect("validated knowledge statement"),
        ))
    } else {
        Some(source_file_or_synthetic(
            drawer_id,
            request.source_file.as_deref().or(request.source.as_deref()),
        ))
    };

    Drawer {
        id: drawer_id.to_string(),
        content: content.to_string(),
        wing: request.wing.clone(),
        room: request.room.clone(),
        source_file,
        source_type: source_confidence.source_type,
        confidence: source_confidence.confidence,
        added_at: iso_timestamp(),
        chunk_index: Some(chunk_idx as i64),
        normalize_version: CURRENT_NORMALIZE_VERSION,
        importance,
        memory_kind: metadata.memory_kind,
        domain: metadata.domain,
        field: metadata.field.clone(),
        anchor_kind: metadata.anchor_kind.clone(),
        anchor_id: metadata.anchor_id.clone(),
        parent_anchor_id: metadata.parent_anchor_id.clone(),
        provenance: metadata.provenance,
        statement: metadata.statement.clone(),
        tier: metadata.tier.clone(),
        status: metadata.status.clone(),
        supporting_refs: metadata.supporting_refs.clone(),
        counterexample_refs: metadata.counterexample_refs.clone(),
        teaching_refs: metadata.teaching_refs.clone(),
        verification_refs: metadata.verification_refs.clone(),
        scope_constraints: metadata.scope_constraints.clone(),
        trigger_hints: metadata.trigger_hints.clone(),
        is_pinned: metadata.is_pinned,
        pin_order: None,
        supersedes: None,
        effective_importance: importance as f64,
        compacted_into: None,
    }
}

#[allow(dead_code)]
fn validate_anchor_metadata(
    request: &IngestRequest,
    domain: &MemoryDomain,
    source_type: &SourceType,
) -> std::result::Result<DerivedAnchor, ErrorData> {
    let explicit_kind = trim_to_option(request.anchor_kind.as_deref());
    let explicit_id = trim_to_option(request.anchor_id.as_deref());

    let anchor = match (explicit_kind, explicit_id) {
        (Some(kind), Some(anchor_id)) => {
            let anchor_kind = parse_anchor_kind(Some(kind))?.expect("explicit kind");
            anchor::validate_explicit_anchor(&anchor_kind, anchor_id).map_err(anchor_error)?;
            DerivedAnchor {
                anchor_kind,
                anchor_id: anchor_id.to_string(),
                parent_anchor_id: trim_to_owned(request.parent_anchor_id.as_deref()),
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err(ErrorData::invalid_params(
                "anchor_kind and anchor_id must be provided together",
                None,
            ));
        }
        (None, None) => {
            if let Some(cwd) = trim_to_option(request.cwd.as_deref()) {
                anchor::derive_anchor_from_cwd(Some(Path::new(cwd))).map_err(anchor_error)?
            } else {
                let defaults = anchor::bootstrap_defaults(source_type);
                DerivedAnchor {
                    anchor_kind: defaults.anchor_kind,
                    anchor_id: defaults.anchor_id,
                    parent_anchor_id: defaults.parent_anchor_id,
                }
            }
        }
    };

    anchor::validate_anchor_domain(domain, &anchor.anchor_kind)
        .map_err(|message| ErrorData::invalid_params(message.to_string(), None))?;
    Ok(anchor)
}

#[allow(dead_code)]
fn validate_tier_status(
    tier: &KnowledgeTier,
    status: &KnowledgeStatus,
) -> std::result::Result<(), ErrorData> {
    let allowed = match tier {
        KnowledgeTier::DaoTian => &[KnowledgeStatus::Canonical, KnowledgeStatus::Demoted][..],
        KnowledgeTier::DaoRen => &[
            KnowledgeStatus::PendingReview,
            KnowledgeStatus::Candidate,
            KnowledgeStatus::Promoted,
            KnowledgeStatus::Demoted,
            KnowledgeStatus::Retired,
        ][..],
        KnowledgeTier::Shu => &[
            KnowledgeStatus::PendingReview,
            KnowledgeStatus::Promoted,
            KnowledgeStatus::Demoted,
            KnowledgeStatus::Retired,
        ][..],
        KnowledgeTier::Qi => &[
            KnowledgeStatus::PendingReview,
            KnowledgeStatus::Candidate,
            KnowledgeStatus::Promoted,
            KnowledgeStatus::Demoted,
            KnowledgeStatus::Retired,
        ][..],
    };

    if allowed.contains(status) {
        return Ok(());
    }

    let message = match tier {
        KnowledgeTier::DaoTian => "dao_tian only allows canonical or demoted",
        KnowledgeTier::DaoRen => "dao_ren only allows candidate, promoted, demoted, or retired",
        KnowledgeTier::Shu => "shu only allows promoted, demoted, or retired",
        KnowledgeTier::Qi => "qi only allows candidate, promoted, demoted, or retired",
    };
    Err(ErrorData::invalid_params(message, None))
}

#[allow(dead_code)]
fn parse_memory_kind(value: Option<&str>) -> std::result::Result<Option<MemoryKind>, ErrorData> {
    parse_enum(value, "memory_kind", |normalized| normalized.parse().ok())
}

fn parse_domain(value: Option<&str>) -> std::result::Result<Option<MemoryDomain>, ErrorData> {
    parse_enum(value, "domain", |normalized| match normalized {
        "project" => Some(MemoryDomain::Project),
        "user" => Some(MemoryDomain::User),
        "agent" => Some(MemoryDomain::Agent),
        "skill" => Some(MemoryDomain::Skill),
        "global" => Some(MemoryDomain::Global),
        _ => None,
    })
}

#[allow(dead_code)]
fn parse_anchor_kind(value: Option<&str>) -> std::result::Result<Option<AnchorKind>, ErrorData> {
    parse_enum(value, "anchor_kind", |normalized| match normalized {
        "global" => Some(AnchorKind::Global),
        "repo" => Some(AnchorKind::Repo),
        "worktree" => Some(AnchorKind::Worktree),
        _ => None,
    })
}

#[allow(dead_code)]
fn parse_provenance(value: Option<&str>) -> std::result::Result<Option<Provenance>, ErrorData> {
    parse_enum(value, "provenance", |normalized| match normalized {
        "runtime" => Some(Provenance::Runtime),
        "research" => Some(Provenance::Research),
        "human" => Some(Provenance::Human),
        _ => None,
    })
}

#[allow(dead_code)]
fn parse_tier(value: Option<&str>) -> std::result::Result<Option<KnowledgeTier>, ErrorData> {
    parse_enum(value, "tier", |normalized| match normalized {
        "qi" => Some(KnowledgeTier::Qi),
        "shu" => Some(KnowledgeTier::Shu),
        "dao_ren" => Some(KnowledgeTier::DaoRen),
        "dao_tian" => Some(KnowledgeTier::DaoTian),
        _ => None,
    })
}

#[allow(dead_code)]
fn parse_status(value: Option<&str>) -> std::result::Result<Option<KnowledgeStatus>, ErrorData> {
    parse_enum(value, "status", |normalized| match normalized {
        "active" => Some(KnowledgeStatus::Active),
        "superseded" => Some(KnowledgeStatus::Superseded),
        "pending_review" => Some(KnowledgeStatus::PendingReview),
        "candidate" => Some(KnowledgeStatus::Candidate),
        "promoted" => Some(KnowledgeStatus::Promoted),
        "canonical" => Some(KnowledgeStatus::Canonical),
        "demoted" => Some(KnowledgeStatus::Demoted),
        "retired" => Some(KnowledgeStatus::Retired),
        _ => None,
    })
}

fn parse_enum<T, F>(
    value: Option<&str>,
    field: &'static str,
    parser: F,
) -> std::result::Result<Option<T>, ErrorData>
where
    F: Fn(&str) -> Option<T>,
{
    let Some(value) = trim_to_option(value) else {
        return Ok(None);
    };

    parser(value)
        .map(Some)
        .ok_or_else(|| ErrorData::invalid_params(format!("invalid {field}: {value}"), None))
}

#[derive(Debug, Clone)]
struct EffectiveMcpSearchScope {
    wing: Option<String>,
    room: Option<String>,
    project_id: Option<String>,
    include_global: bool,
    all_projects: bool,
    filters: SearchFilters,
}

#[derive(Debug, Clone)]
struct EffectiveMcpContextScope {
    project_id: Option<String>,
    all_projects: bool,
    domain: Option<String>,
    field: Option<String>,
}

fn merge_scope_string(
    scope_value: Option<&String>,
    legacy_value: Option<&String>,
    field: &str,
) -> std::result::Result<Option<String>, ErrorData> {
    match (scope_value, legacy_value) {
        (Some(scope), Some(legacy)) if scope != legacy => Err(ErrorData::invalid_params(
            format!("scope.{field} conflicts with legacy top-level {field}"),
            None,
        )),
        (Some(scope), _) => Ok(Some(scope.clone())),
        (None, Some(legacy)) => Ok(Some(legacy.clone())),
        (None, None) => Ok(None),
    }
}

fn merge_scope_bool(
    scope_value: Option<bool>,
    legacy_value: Option<bool>,
    field: &str,
) -> std::result::Result<Option<bool>, ErrorData> {
    match (scope_value, legacy_value) {
        (Some(scope), Some(legacy)) if scope != legacy => Err(ErrorData::invalid_params(
            format!("scope.{field} conflicts with legacy top-level {field}"),
            None,
        )),
        (Some(scope), _) => Ok(Some(scope)),
        (None, Some(legacy)) => Ok(Some(legacy)),
        (None, None) => Ok(None),
    }
}

fn merge_room_and_session(
    room: Option<String>,
    session: Option<&String>,
) -> std::result::Result<Option<String>, ErrorData> {
    match (room, session) {
        (Some(room), Some(session)) if room != *session => Err(ErrorData::invalid_params(
            "scope.session conflicts with room; session maps to the drawer room column",
            None,
        )),
        (Some(room), _) => Ok(Some(room)),
        (None, Some(session)) => Ok(Some(session.clone())),
        (None, None) => Ok(None),
    }
}

fn effective_search_scope(
    request: &SearchRequest,
) -> std::result::Result<EffectiveMcpSearchScope, ErrorData> {
    let scope = request.scope.as_ref();
    let wing = merge_scope_string(
        scope.and_then(|value| value.wing.as_ref()),
        request.wing.as_ref(),
        "wing",
    )?;
    let room = merge_scope_string(
        scope.and_then(|value| value.room.as_ref()),
        request.room.as_ref(),
        "room",
    )?;
    let room = merge_room_and_session(room, scope.and_then(|value| value.session.as_ref()))?;
    let project_id = merge_scope_string(
        scope.and_then(|value| value.project_id.as_ref()),
        request.project_id.as_ref(),
        "project_id",
    )?;
    let include_global = merge_scope_bool(
        scope.and_then(|value| value.include_global),
        request.include_global,
        "include_global",
    )?
    .unwrap_or(false);
    let all_projects = merge_scope_bool(
        scope.and_then(|value| value.all_projects),
        request.all_projects,
        "all_projects",
    )?
    .unwrap_or(false);
    let filters = SearchFilters {
        memory_kind: merge_scope_string(
            scope.and_then(|value| value.memory_kind.as_ref()),
            request.memory_kind.as_ref(),
            "memory_kind",
        )?,
        domain: merge_scope_string(
            scope.and_then(|value| value.domain.as_ref()),
            request.domain.as_ref(),
            "domain",
        )?,
        field: merge_scope_string(
            scope.and_then(|value| value.field.as_ref()),
            request.field.as_ref(),
            "field",
        )?,
        tier: merge_scope_string(
            scope.and_then(|value| value.tier.as_ref()),
            request.tier.as_ref(),
            "tier",
        )?,
        status: merge_scope_string(
            scope.and_then(|value| value.status.as_ref()),
            request.status.as_ref(),
            "status",
        )?,
        anchor_kind: merge_scope_string(
            scope.and_then(|value| value.anchor_kind.as_ref()),
            request.anchor_kind.as_ref(),
            "anchor_kind",
        )?,
    };
    Ok(EffectiveMcpSearchScope {
        wing,
        room,
        project_id,
        include_global,
        all_projects,
        filters,
    })
}

fn reject_context_search_only_scope(
    scope: &RetrievalScopeRequest,
) -> std::result::Result<(), ErrorData> {
    for (field, present) in [
        ("wing", scope.wing.is_some()),
        ("room", scope.room.is_some()),
        ("session", scope.session.is_some()),
        ("include_global", scope.include_global.unwrap_or(false)),
        ("memory_kind", scope.memory_kind.is_some()),
        ("tier", scope.tier.is_some()),
        ("status", scope.status.is_some()),
        ("anchor_kind", scope.anchor_kind.is_some()),
    ] {
        if present {
            return Err(ErrorData::invalid_params(
                format!("scope.{field} is supported by mempal_search, not mempal_context"),
                None,
            ));
        }
    }
    Ok(())
}

fn effective_context_scope(
    request: &ContextRequest,
) -> std::result::Result<EffectiveMcpContextScope, ErrorData> {
    let scope = request.scope.as_ref();
    if let Some(scope) = scope {
        reject_context_search_only_scope(scope)?;
    }
    Ok(EffectiveMcpContextScope {
        project_id: merge_scope_string(
            scope.and_then(|value| value.project_id.as_ref()),
            request.project_id.as_ref(),
            "project_id",
        )?,
        all_projects: merge_scope_bool(
            scope.and_then(|value| value.all_projects),
            request.all_projects,
            "all_projects",
        )?
        .unwrap_or(false),
        domain: merge_scope_string(
            scope.and_then(|value| value.domain.as_ref()),
            request.domain.as_ref(),
            "domain",
        )?,
        field: merge_scope_string(
            scope.and_then(|value| value.field.as_ref()),
            request.field.as_ref(),
            "field",
        )?,
    })
}

fn reject_unresolved_strict_context_scope(
    project_id: Option<&str>,
    all_projects: bool,
    config: &Config,
) -> std::result::Result<(), ErrorData> {
    if project_id.is_none() && !all_projects && config.search.strict_project_isolation {
        return Err(ErrorData::invalid_params(
            "no project scope resolved, isolation strict; set scope.all_projects=true to opt into all-project context",
            None,
        ));
    }
    Ok(())
}

#[allow(dead_code)]
fn normalize_refs(values: Option<&[String]>) -> Vec<String> {
    values
        .unwrap_or(&[])
        .iter()
        .filter_map(|value| trim_to_owned(Some(value.as_str())))
        .collect()
}

#[allow(dead_code)]
fn validate_drawer_refs(field: &str, values: &[String]) -> std::result::Result<(), ErrorData> {
    if values.iter().all(|value| looks_like_drawer_id(value)) {
        Ok(())
    } else {
        Err(ErrorData::invalid_params(
            format!("{field} must contain drawer ids"),
            None,
        ))
    }
}

#[allow(dead_code)]
fn looks_like_drawer_id(value: &str) -> bool {
    value.starts_with("drawer_")
        && value.len() > "drawer_".len()
        && value
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn trigger_hints_from_dto(dto: &TriggerHintsDto) -> TriggerHints {
    TriggerHints {
        intent_tags: normalize_refs(Some(&dto.intent_tags)),
        workflow_bias: normalize_refs(Some(&dto.workflow_bias)),
        tool_needs: normalize_refs(Some(&dto.tool_needs)),
    }
}

fn trim_to_option(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn trim_to_owned(value: Option<&str>) -> Option<String> {
    trim_to_option(value).map(ToOwned::to_owned)
}

fn required_string<'a>(
    value: Option<&'a str>,
    field: &'static str,
) -> std::result::Result<&'a str, ErrorData> {
    trim_to_option(value)
        .ok_or_else(|| ErrorData::invalid_params(format!("{field} is required"), None))
}

fn parse_runtime_adoption_track_opt(
    value: Option<&str>,
) -> std::result::Result<Option<RuntimeAdoptionTrack>, ErrorData> {
    parse_enum(value, "track", |normalized| match normalized {
        "runtime_adoption" => Some(RuntimeAdoptionTrack::RuntimeAdoption),
        "card_context" => Some(RuntimeAdoptionTrack::CardContext),
        "card_embedding" => Some(RuntimeAdoptionTrack::CardEmbedding),
        "evaluator" => Some(RuntimeAdoptionTrack::Evaluator),
        "research_adapter" => Some(RuntimeAdoptionTrack::ResearchAdapter),
        _ => None,
    })
}

fn parse_runtime_adoption_track(
    value: &str,
) -> std::result::Result<RuntimeAdoptionTrack, ErrorData> {
    parse_runtime_adoption_track_opt(Some(value))?
        .ok_or_else(|| ErrorData::invalid_params("track is required", None))
}

fn parse_runtime_adoption_signal(
    value: &str,
) -> std::result::Result<RuntimeAdoptionSignal, ErrorData> {
    parse_enum(Some(value), "signal", |normalized| match normalized {
        "used" => Some(RuntimeAdoptionSignal::Used),
        "accepted" => Some(RuntimeAdoptionSignal::Accepted),
        "rejected" => Some(RuntimeAdoptionSignal::Rejected),
        "miss" => Some(RuntimeAdoptionSignal::Miss),
        "rollback" => Some(RuntimeAdoptionSignal::Rollback),
        "contradiction" => Some(RuntimeAdoptionSignal::Contradiction),
        "neutral" => Some(RuntimeAdoptionSignal::Neutral),
        _ => None,
    })?
    .ok_or_else(|| ErrorData::invalid_params("signal is required", None))
}

fn runtime_adoption_track_slug(track: &RuntimeAdoptionTrack) -> &'static str {
    match track {
        RuntimeAdoptionTrack::RuntimeAdoption => "runtime_adoption",
        RuntimeAdoptionTrack::CardContext => "card_context",
        RuntimeAdoptionTrack::CardEmbedding => "card_embedding",
        RuntimeAdoptionTrack::Evaluator => "evaluator",
        RuntimeAdoptionTrack::ResearchAdapter => "research_adapter",
    }
}

fn runtime_adoption_signal_slug(signal: &RuntimeAdoptionSignal) -> &'static str {
    match signal {
        RuntimeAdoptionSignal::Used => "used",
        RuntimeAdoptionSignal::Accepted => "accepted",
        RuntimeAdoptionSignal::Rejected => "rejected",
        RuntimeAdoptionSignal::Miss => "miss",
        RuntimeAdoptionSignal::Rollback => "rollback",
        RuntimeAdoptionSignal::Contradiction => "contradiction",
        RuntimeAdoptionSignal::Neutral => "neutral",
    }
}

fn phase3_event_id(
    track: &RuntimeAdoptionTrack,
    signal: &RuntimeAdoptionSignal,
    feature: &str,
) -> String {
    let signal = match signal {
        RuntimeAdoptionSignal::Used => "used",
        RuntimeAdoptionSignal::Accepted => "accepted",
        RuntimeAdoptionSignal::Rejected => "rejected",
        RuntimeAdoptionSignal::Miss => "miss",
        RuntimeAdoptionSignal::Rollback => "rollback",
        RuntimeAdoptionSignal::Contradiction => "contradiction",
        RuntimeAdoptionSignal::Neutral => "neutral",
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sanitized_feature = feature
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!(
        "adoption_{}_{}_{}_{}",
        runtime_adoption_track_slug(track),
        signal,
        sanitized_feature,
        nanos
    )
}

fn runtime_adoption_stats(events: &[RuntimeAdoptionEvent]) -> RuntimeAdoptionStatsDto {
    let mut stats = RuntimeAdoptionStatsDto {
        total: events.len(),
        used: 0,
        accepted: 0,
        rejected: 0,
        misses: 0,
        rollbacks: 0,
        contradictions: 0,
        neutral: 0,
    };
    for event in events {
        match event.signal {
            RuntimeAdoptionSignal::Used => stats.used += 1,
            RuntimeAdoptionSignal::Accepted => stats.accepted += 1,
            RuntimeAdoptionSignal::Rejected => stats.rejected += 1,
            RuntimeAdoptionSignal::Miss => stats.misses += 1,
            RuntimeAdoptionSignal::Rollback => stats.rollbacks += 1,
            RuntimeAdoptionSignal::Contradiction => stats.contradictions += 1,
            RuntimeAdoptionSignal::Neutral => stats.neutral += 1,
        }
    }
    stats
}

fn phase3_gate_report(
    db: &Database,
    candidate: &str,
) -> std::result::Result<Phase3GateDto, ErrorData> {
    let (track, ready_fn): (RuntimeAdoptionTrack, fn(&RuntimeAdoptionStatsDto) -> bool) =
        match candidate {
            "card-context-default" => (RuntimeAdoptionTrack::CardContext, |stats| {
                stats.accepted >= 3 && stats.rollbacks == 0 && stats.rejected <= stats.accepted
            }),
            "card-embeddings" => (RuntimeAdoptionTrack::CardEmbedding, |stats| {
                stats.misses >= 3 && stats.rollbacks == 0
            }),
            "evaluator-api" => (RuntimeAdoptionTrack::Evaluator, |stats| {
                stats.accepted >= 3 && stats.rollbacks == 0 && stats.contradictions == 0
            }),
            "research-adapter" => (RuntimeAdoptionTrack::ResearchAdapter, |stats| {
                stats.accepted >= 1 && stats.contradictions == 0 && stats.rollbacks == 0
            }),
            other => {
                return Err(ErrorData::invalid_params(
                    format!("unsupported phase3 candidate: {other}"),
                    None,
                ));
            }
        };
    let events = db
        .list_runtime_adoption_events(
            &RuntimeAdoptionFilter {
                track: Some(track.clone()),
                feature: None,
            },
            10_000,
        )
        .map_err(|error| {
            ErrorData::internal_error(
                format!("failed to list runtime adoption events: {error}"),
                None,
            )
        })?;
    let stats = runtime_adoption_stats(&events);
    let ready = ready_fn(&stats);
    let mut reasons = Vec::new();
    if ready {
        reasons.push("minimum evidence threshold satisfied".to_string());
    } else {
        reasons.push("minimum evidence threshold not satisfied".to_string());
    }
    if stats.rollbacks > 0 {
        reasons.push("rollback signals block default or authority changes".to_string());
    }
    if stats.contradictions > 0 {
        reasons.push("contradiction signals require review before implementation".to_string());
    }
    Ok(Phase3GateDto {
        candidate: candidate.to_string(),
        ready,
        required_track: runtime_adoption_track_slug(&track).to_string(),
        stats,
        reasons,
    })
}

fn validate_research_adapter_plan_value(value: &serde_json::Value) -> ResearchAdapterPlanDto {
    let mut errors = Vec::new();
    let report_id = required_json_string(value, "report_id", &mut errors);
    let title = required_json_string(value, "title", &mut errors);
    let source_count = value
        .get("sources")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    if source_count == 0 {
        errors.push("sources must contain at least one item".to_string());
    }
    let finding_count = value
        .get("findings")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    if finding_count == 0 {
        errors.push("findings must contain at least one item".to_string());
    }
    let candidate_insight_count = value
        .get("candidate_insights")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);

    ResearchAdapterPlanDto {
        valid: errors.is_empty(),
        report_id,
        title,
        source_count,
        finding_count,
        candidate_insight_count,
        errors,
    }
}

fn required_json_string(
    value: &serde_json::Value,
    field: &'static str,
    errors: &mut Vec<String>,
) -> String {
    match value.get(field).and_then(serde_json::Value::as_str) {
        Some(raw) if !raw.trim().is_empty() => raw.trim().to_string(),
        _ => {
            errors.push(format!("{field} is required"));
            String::new()
        }
    }
}

fn anchor_error(error: anchor::AnchorError) -> ErrorData {
    ErrorData::invalid_params(error.to_string(), None)
}

// The tool_router impl block and all tool handlers are defined below.
// Tools from fork: status, search, timeline, read_drawer, read_drawers,
//   ingest (with gating/novelty/chunking/privacy), delete, rollback,
//   taxonomy, kg, tunnels (full CRUD), peek_partner, cowork_push, fact_check
// Tools from upstream: context, knowledge_distill, knowledge_gate,
//   knowledge_policy, knowledge_promote, knowledge_demote,
//   knowledge_publish_anchor, field_taxonomy

#[tool_router(router = tool_router)]
impl MempalMcpServer {
    #[tool(
        name = "mempal_status",
        description = "Return compact health/status by default. Use detail=\"full\" to include the memory protocol and AAAK spec, and scope=\"all\" to include all-project scope counts."
    )]
    pub async fn mempal_status_tool(
        &self,
        Parameters(request): Parameters<StatusRequest>,
    ) -> std::result::Result<Json<StatusResponse>, ErrorData> {
        let detail = request.detail.unwrap_or_default();
        let scope_mode = request.scope.unwrap_or(match detail {
            StatusDetail::Compact => StatusScope::Project,
            StatusDetail::Full => StatusScope::All,
        });
        let cfg_meta = ConfigHandle::snapshot_meta();
        let config = self.status_config_snapshot();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let unresolved_project_scope = scope_mode == StatusScope::Project && project_id.is_none();
        let project_scope = match scope_mode {
            StatusScope::All => ProjectSearchScope::all_projects(),
            StatusScope::Project => match project_id {
                Some(project_id) => ProjectSearchScope {
                    project_id: Some(project_id),
                    mode: crate::core::project::ProjectFilterMode::ProjectScoped,
                },
                None => ProjectSearchScope {
                    project_id: None,
                    mode: crate::core::project::ProjectFilterMode::NullOnly,
                },
            },
        };
        let mut system_warnings = current_system_warnings();
        let mut database_diagnostic = None;
        let queue_stats = match self.async_queue.stats().await {
            Ok(stats) => stats,
            Err(error) => {
                let diagnostic = status_database_diagnostic(&self.db_path, "queue_stats", &error);
                record_status_database_diagnostic(
                    &mut system_warnings,
                    &mut database_diagnostic,
                    diagnostic,
                );
                default_queue_stats()
            }
        };
        let db_snapshot = match self
            .load_status_db_snapshot(project_scope, config.turns.clone())
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let diagnostic =
                    status_database_diagnostic(&self.db_path, "status_snapshot", error.as_ref());
                record_status_database_diagnostic(
                    &mut system_warnings,
                    &mut database_diagnostic,
                    diagnostic,
                );
                StatusDbSnapshot::default()
            }
        };
        let endpoint_health = crate::endpoint_health::probe_endpoints(config.as_ref()).await;
        let embed_snapshot = global_embed_status().snapshot();
        let embed_endpoint_runtime = global_embed_status()
            .endpoint_runtime_snapshots()
            .into_iter()
            .map(|snapshot| (snapshot.id.clone(), snapshot))
            .collect::<BTreeMap<_, _>>();
        let intelligence_snapshot = crate::intelligence::global_intelligence_status().snapshot();
        let db_holders = crate::process_diagnostics::inspect_db_holders(&self.db_path);
        let vector_search_circuit =
            VectorSearchCircuit::from_config_and_snapshot(config.as_ref(), &embed_snapshot);
        if db_snapshot.null_project_backfill_pending > 0 {
            system_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: format!(
                    "{} drawers still have NULL project_id; run `mempal project migrate --project <id>` to backfill historical records",
                    db_snapshot.null_project_backfill_pending
                ),
                source: "project_isolation".to_string(),
            });
        }
        if let Some(warning) = stale_index_warning_from_bool(db_snapshot.vector_index_stale) {
            system_warnings.push(warning);
        }
        if db_snapshot.vector_index_empty {
            system_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: format!(
                    "drawer_vectors index is empty ({} vectors for {} drawers); vector recall is disabled (BM25-only) until `mempal reindex --from-config` repopulates it",
                    db_snapshot.vector_rows, db_snapshot.drawer_count
                ),
                source: "vector_index".to_string(),
            });
        }
        if db_snapshot.design_insight_summary.high_value_open > 0 {
            system_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: format!(
                    "{} unresolved high-value design insight(s) need draining; run `mempal insight list --status open --min-priority 4`",
                    db_snapshot.design_insight_summary.high_value_open
                ),
                source: "design_insights".to_string(),
            });
        }
        if unresolved_project_scope && config.search.strict_project_isolation {
            system_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: "no project scope resolved, isolation strict".to_string(),
                source: "project_isolation".to_string(),
            });
        }
        push_model_backend_warnings(
            &mut system_warnings,
            config.as_ref(),
            &endpoint_health,
            &queue_stats,
        );
        push_db_holder_warnings(&mut system_warnings, &db_holders);

        let embed_failure_headline =
            crate::core::queue::failure_headline_count(embed_snapshot.fail_count, &queue_stats);
        let config_for_gating_status = Arc::clone(&config);
        let restart_required_config_changes = ConfigHandle::restart_required_pending();
        let ingest_gating_status = match self.async_db().await {
            Ok(async_db) => match async_db
                .run_read_anyhow(move |db| {
                    let gating_drop_counts = db
                        .gating_drop_counts()
                        .context("gating drop counts failed")?;
                    let dropped_total = gating_drop_counts.total.unwrap_or_else(|| {
                        gating_drop_counts.by_reason.values().copied().sum::<u64>()
                    });
                    crate::observability::gating_runtime_status(
                        db,
                        config_for_gating_status.as_ref(),
                        dropped_total,
                        restart_required_config_changes,
                    )
                })
                .await
            {
                Ok(status) => GatingRuntimeStatusDto::from(status),
                Err(error) => {
                    let diagnostic = status_database_diagnostic(
                        &self.db_path,
                        "ingest_gating_status",
                        error.as_ref(),
                    );
                    record_status_database_diagnostic(
                        &mut system_warnings,
                        &mut database_diagnostic,
                        diagnostic,
                    );
                    GatingRuntimeStatusDto::default()
                }
            },
            Err(error) => {
                let diagnostic =
                    status_database_diagnostic(&self.db_path, "async_db", error.as_ref());
                record_status_database_diagnostic(
                    &mut system_warnings,
                    &mut database_diagnostic,
                    diagnostic,
                );
                GatingRuntimeStatusDto::default()
            }
        };
        let remote_call_policy = &config.privacy.remote_calls;
        let embed_endpoints = config.embed.effective_endpoints().unwrap_or_default();
        let embedding_endpoint_label = |base_url: &str| {
            endpoint_policy_diagnostic_label(
                remote_call_policy,
                RemoteCallService::Embedding,
                base_url,
            )
        };
        let llm_endpoint_label = |base_url: &str| {
            endpoint_policy_diagnostic_label(remote_call_policy, RemoteCallService::Llm, base_url)
        };

        Ok(Json(StatusResponse {
            schema_version: db_snapshot.schema_version,
            fork_ext_version: db_snapshot.fork_ext_version,
            normalize_version_current: CURRENT_NORMALIZE_VERSION,
            stale_drawer_count: db_snapshot.stale_drawer_count,
            vector_index_stale: db_snapshot.vector_index_stale,
            vector_rows: db_snapshot.vector_rows,
            vector_index_empty: db_snapshot.vector_index_empty,
            search_decay_mode: config.search.decay.mode.to_string(),
            drawer_count: db_snapshot.drawer_count,
            total_compacted_drawers: db_snapshot.total_compacted_drawers,
            consolidation_runs: db_snapshot.consolidation_runs,
            last_consolidation_at: db_snapshot.last_consolidation_at,
            last_sleep_at: db_snapshot.last_sleep_at,
            sleep_items_pruned: db_snapshot.sleep_items_pruned,
            sleep_items_compacted: db_snapshot.sleep_items_compacted,
            sleep_conflicts_resolved: db_snapshot.sleep_conflicts_resolved,
            pending_card_count: db_snapshot.pending_card_count,
            last_crystallization_at: db_snapshot.last_crystallization_at,
            design_insights: DesignInsightStatusDto {
                open_total: db_snapshot.design_insight_summary.open_total,
                high_value_open: db_snapshot.design_insight_summary.high_value_open,
                open_by_target: db_snapshot.design_insight_summary.open_by_target,
            },
            taxonomy_count: db_snapshot.taxonomy_count,
            db_size_bytes: db_snapshot.db_size_bytes,
            diary_rollup_days: db_snapshot.diary_rollup_days,
            config_version: cfg_meta.version,
            config_loaded_at_unix_ms: cfg_meta.loaded_at_unix_ms,
            scopes: db_snapshot.scopes,
            source_type_distribution: db_snapshot.source_type_distribution,
            pinned_fact_counts: db_snapshot.pinned_fact_counts,
            aaak_spec: match detail {
                StatusDetail::Compact => String::new(),
                StatusDetail::Full => crate::aaak::generate_spec(),
            },
            memory_protocol: match detail {
                StatusDetail::Compact => String::new(),
                StatusDetail::Full => crate::core::protocol::MEMORY_PROTOCOL.to_string(),
            },
            endpoint_health: EndpointHealthDto {
                embedding_reachable: endpoint_health.embedding.reachable,
                embedding_latency_ms: endpoint_health.embedding.latency_ms,
                embedding_detail: endpoint_health.embedding.detail.clone(),
                llm_reachable: endpoint_health.llm.reachable,
                llm_latency_ms: endpoint_health.llm.latency_ms,
                llm_control_plane_reachable: endpoint_health.llm_control_plane.reachable,
                llm_control_plane_latency_ms: endpoint_health.llm_control_plane.latency_ms,
                llm_control_plane_detail: endpoint_health.llm_control_plane.detail.clone(),
                llm_generation_reachable: endpoint_health.llm_generation.reachable,
                llm_generation_latency_ms: endpoint_health.llm_generation.latency_ms,
                llm_generation_detail: endpoint_health.llm_generation.detail.clone(),
            },
            embed_status: EmbedStatusDto {
                backend: config.embed.backend.clone(),
                base_url: config
                    .embed
                    .resolved_openai_base_url()
                    .map(embedding_endpoint_label),
                model: config.embed.effective_model_summary(),
                endpoints: embed_endpoints
                    .iter()
                    .map(|endpoint| {
                        let runtime = embed_endpoint_runtime.get(&endpoint.id);
                        let last_error = endpoint_policy_runtime_error(
                            remote_call_policy,
                            RemoteCallService::Embedding,
                            &endpoint.base_url,
                            runtime.and_then(|state| state.last_error.clone()),
                        );
                        EmbedEndpointStatusDto {
                            id: endpoint.id.clone(),
                            backend: endpoint.backend.clone(),
                            base_url: embedding_endpoint_label(&endpoint.base_url),
                            model: endpoint.model.clone(),
                            priority: endpoint.priority,
                            retry_interval_secs: endpoint.retry_interval_secs,
                            request_timeout_secs: endpoint.request_timeout_secs,
                            max_concurrent: endpoint.max_concurrent,
                            dimensions: endpoint.dimensions,
                            cooldown_remaining_secs: runtime
                                .and_then(|state| state.cooldown_remaining_secs),
                            cooldown_until_unix_ms: runtime
                                .and_then(|state| state.cooldown_until_unix_ms),
                            last_failure_at_unix_ms: runtime
                                .and_then(|state| state.last_failure_at_unix_ms),
                            last_success_at_unix_ms: runtime
                                .and_then(|state| state.last_success_at_unix_ms),
                            last_error,
                        }
                    })
                    .collect(),
                max_concurrent: config.embed.pool_capacity(),
                pending_count: queue_stats.pending,
                claimed_count: queue_stats.claimed,
                failed_count: queue_stats.failed,
                degraded: embed_snapshot.degraded,
                fail_count: embed_failure_headline,
                failure_count: embed_failure_headline,
                last_error: endpoint_policy_global_runtime_error(
                    remote_call_policy,
                    RemoteCallService::Embedding,
                    embed_endpoints
                        .iter()
                        .map(|endpoint| endpoint.base_url.as_str()),
                    embed_snapshot.last_error,
                ),
                last_success_at_unix_ms: embed_snapshot.last_success_at_unix_ms,
            },
            embedder_circuit: EmbedderCircuitDto {
                open: vector_search_circuit.open,
                failure_count: vector_search_circuit.failure_count,
                failure_threshold: vector_search_circuit.failure_threshold,
                bm25_fallback_enabled: vector_search_circuit.bm25_fallback_enabled,
                search_deadline_secs: vector_search_circuit.search_deadline_secs,
                vector_search_mode: vector_search_circuit
                    .vector_search_mode
                    .as_str()
                    .to_string(),
            },
            ingest_gating_status,
            queue_stats: QueueStatsDto {
                pending: queue_stats.pending,
                claimed: queue_stats.claimed,
                failed: queue_stats.failed,
                failed_retryable: queue_stats.failed_retryable,
                failed_terminal: queue_stats.failed_terminal,
                failed_retryable_embed: queue_stats.failed_retryable_embed,
                failed_retryable_llm: queue_stats.failed_retryable_llm,
                last_auto_requeue_at_unix_ms: queue_stats.last_auto_requeue_at_unix_ms,
                rate_per_min: queue_stats.rate_per_min,
                oldest_pending_age_secs: queue_stats.oldest_pending_age_secs,
                avg_processing_ms: queue_stats.avg_processing_ms,
                eta_secs: queue_stats.eta_secs,
            },
            db_holders,
            scrub_stats: ScrubStatsDto::from(ConfigHandle::scrub_stats()),
            chunker_stats: ChunkerStatsDto::from(
                crate::ingest::chunk::global_chunker_stats().snapshot(),
            ),
            llm_status: LlmStatusDto {
                enabled: config.llm.enabled,
                backend: Some(config.llm.backend.clone()),
                model: config.llm.effective_model_summary(),
                endpoints: config
                    .llm
                    .effective_endpoints()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|endpoint| LlmEndpointStatusDto {
                        id: endpoint.id,
                        base_url: llm_endpoint_label(&endpoint.base_url),
                        model: endpoint.model,
                        priority: endpoint.priority,
                        retry_interval_secs: endpoint.retry_interval_secs,
                        max_concurrent: endpoint.max_concurrent,
                    })
                    .collect(),
                max_concurrent: config.llm.pool_capacity(),
            },
            intelligence_status: IntelligenceStatusDto {
                mode: config.memory_intelligence.mode.to_string(),
                llm_state: intelligence_llm_state(config.as_ref(), endpoint_health.llm.reachable),
                last_success_at_unix_ms: intelligence_snapshot.last_success_at_unix_ms,
                failure_count: intelligence_snapshot.failure_count,
                last_error: intelligence_snapshot.last_error,
            },
            turn_storage: TurnStorageStatusDto {
                storage_mode: config.turns.storage_mode.to_string(),
                default_importance: config.turns.default_importance,
                raw_turn_count: db_snapshot.raw_turn_count,
                raw_turn_wings: config.turns.raw_turn_wings.clone(),
                raw_turn_rooms: config.turns.raw_turn_rooms.clone(),
            },
            database_diagnostic,
            system_warnings,
        }))
    }

    #[tool(
        name = "mempal_pinned_facts",
        description = "Return canonical pinned facts for prompt injection without running embedding search. Use this at session start for always-on context such as user preferences and durable constraints. Results are pure SQL, project-scoped, ordered by pin_order/importance, capped by budget_chars, and include typed metadata plus citations."
    )]
    pub async fn mempal_pinned_facts(
        &self,
        Parameters(request): Parameters<PinnedFactsRequest>,
    ) -> std::result::Result<Json<PinnedFactsResponse>, ErrorData> {
        let config = ConfigHandle::current();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let budget_chars = request.budget_chars.unwrap_or(4_000);
        let db = self.open_db()?;
        let drawers = db
            .get_pinned_facts(project_id.as_deref(), budget_chars)
            .map_err(db_error)?;
        let used_chars = drawers
            .iter()
            .map(|drawer| drawer.content.chars().count())
            .sum();
        let text = format_pinned_facts_text(&drawers);
        let facts = drawers.into_iter().map(PinnedFactDto::from).collect();

        Ok(Json(PinnedFactsResponse {
            project_id,
            budget_chars,
            used_chars,
            text,
            facts,
            system_warnings: current_system_warnings(),
        }))
    }

    #[tool(
        name = "mempal_search",
        description = "Search persistent project memory via hybrid vector+BM25 retrieval. Prefer the unified `scope` object for wing/room/session/project and typed metadata filters; legacy top-level scope fields remain compatibility aliases. PREFER THIS over grepping files or guessing from general knowledge when answering ANY project-specific question. Response search_mode reports hybrid or bm25_only fallback. Every result includes drawer_id and source_file for citation, typed fields (`memory_kind`, `domain`, `field`, status/tier/anchor data), and structured AAAK-derived signals (`entities`, `topics`, `flags`, `emotions`, `importance_stars`) for filtering and ranking."
    )]
    pub async fn mempal_search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> std::result::Result<Json<SearchResponse>, ErrorData> {
        let config = ConfigHandle::current();
        let request_scope = effective_search_scope(&request)?;
        let project_id = self
            .resolve_mcp_project_id(request_scope.project_id.as_deref(), config.as_ref())
            .await?;
        let unresolved_scope = project_id.is_none() && !request_scope.all_projects;
        let scope = ProjectSearchScope::from_request(
            project_id,
            request_scope.include_global,
            request_scope.all_projects,
            config.search.strict_project_isolation,
        );
        let top_k = request.top_k.unwrap_or(10);
        let search_options = SearchOptions {
            filters: request_scope.filters.clone(),
            with_neighbors: request.with_neighbors.unwrap_or(false),
            include_raw_turns: request.include_raw_turns.unwrap_or(false),
            include_expired: request.include_expired.unwrap_or(false),
        };
        let mut extra_warnings = Vec::new();
        let mut search_mode = SearchMode::Hybrid;
        let mut response_warnings = Vec::new();
        let route = {
            let query = request.query.clone();
            let wing = request_scope.wing.clone();
            let room = request_scope.room.clone();
            let fallback_route = crate::core::types::RouteDecision {
                wing: wing.clone(),
                room: room.clone(),
                confidence: if wing.is_some() || room.is_some() {
                    1.0
                } else {
                    0.0
                },
                reason: "bounded MCP fallback: route resolution timed out".to_string(),
            };
            match self
                .run_read_anyhow_bounded(
                    move |db| {
                        resolve_route(db, &query, wing.as_deref(), room.as_deref())
                            .map_err(|error| anyhow::anyhow!("routing failed: {error}"))
                    },
                    self.search_route_deadline,
                )
                .await
            {
                Ok(Some(route)) => route,
                Ok(None) => {
                    push_mcp_timeout_warning(
                        &mut response_warnings,
                        &mut extra_warnings,
                        "search route resolution",
                        self.search_route_deadline,
                    );
                    fallback_route
                }
                Err(error) => match self.handle_search_database_error(
                    error,
                    "search route resolution",
                    &mut response_warnings,
                    &mut extra_warnings,
                )? {
                    Some(route) => route,
                    None => fallback_route,
                },
            }
        };
        let embed_snapshot = global_embed_status().snapshot();
        let results = if config.search.bm25_fallback && embed_snapshot.degraded {
            search_mode = SearchMode::Bm25Only;
            let warning = bm25_fallback_warning_degraded(embed_snapshot.fail_count);
            response_warnings.push(warning.clone());
            extra_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: warning,
                source: "embed".to_string(),
            });
            let query = request.query.clone();
            let route = route.clone();
            let scope = scope.clone();
            let search_options = search_options.clone();
            match self
                .run_bm25_search_bounded(query, route, scope, search_options, top_k)
                .await
            {
                Ok(Some(results)) => results,
                Ok(None) => {
                    push_mcp_timeout_warning(
                        &mut response_warnings,
                        &mut extra_warnings,
                        "BM25 fallback search",
                        self.search_db_deadline,
                    );
                    Vec::new()
                }
                Err(error) => self
                    .handle_search_database_error(
                        error,
                        "BM25 fallback search",
                        &mut response_warnings,
                        &mut extra_warnings,
                    )?
                    .unwrap_or_default(),
            }
        } else {
            let embedder = match self.embedder_factory.build().await {
                Ok(embedder) => Some(embedder),
                Err(error) if config.search.bm25_fallback => {
                    search_mode = SearchMode::Bm25Only;
                    let warning = bm25_fallback_warning_embed_error(
                        &crate::core::config::scrub_sensitive_text(&error.to_string()),
                    );
                    response_warnings.push(warning.clone());
                    extra_warnings.push(SystemWarning {
                        level: "warn".to_string(),
                        message: warning,
                        source: "embed".to_string(),
                    });
                    None
                }
                Err(error) => {
                    return Err(ErrorData::internal_error(
                        format!("failed to build embedder: {error}"),
                        None,
                    ));
                }
            };
            if let Some(embedder) = embedder {
                match tokio::time::timeout(
                    Duration::from_secs(config.embed.retry.search_deadline_secs),
                    embedder.embed(&[request.query.as_str()]),
                )
                .await
                {
                    Ok(Ok(vectors)) => {
                        let query_vector = vectors.into_iter().next().ok_or_else(|| {
                            ErrorData::internal_error("embedder returned no query vector", None)
                        })?;
                        let query = request.query.clone();
                        let hybrid_route = route.clone();
                        let hybrid_scope = scope.clone();
                        let hybrid_options = search_options.clone();
                        match self
                            .run_read_anyhow_bounded(
                                move |db| {
                                    search_with_vector_and_scope_options(
                                        db,
                                        &query,
                                        &query_vector,
                                        hybrid_route,
                                        &hybrid_scope,
                                        hybrid_options,
                                        top_k,
                                    )
                                    .map_err(|error| anyhow::anyhow!("search failed: {error}"))
                                },
                                self.search_db_deadline,
                            )
                            .await
                        {
                            Ok(Some(results)) => results,
                            Ok(None) => {
                                search_mode = SearchMode::Bm25Only;
                                push_mcp_timeout_warning(
                                    &mut response_warnings,
                                    &mut extra_warnings,
                                    "hybrid search",
                                    self.search_db_deadline,
                                );
                                let query = request.query.clone();
                                let route = route.clone();
                                let scope = scope.clone();
                                let search_options = search_options.clone();
                                match self
                                    .run_bm25_search_bounded(
                                        query,
                                        route,
                                        scope,
                                        search_options,
                                        top_k,
                                    )
                                    .await
                                {
                                    Ok(Some(results)) => results,
                                    Ok(None) => {
                                        push_mcp_timeout_warning(
                                            &mut response_warnings,
                                            &mut extra_warnings,
                                            "BM25 fallback search",
                                            self.search_db_deadline,
                                        );
                                        Vec::new()
                                    }
                                    Err(error) => self
                                        .handle_search_database_error(
                                            error,
                                            "BM25 fallback search",
                                            &mut response_warnings,
                                            &mut extra_warnings,
                                        )?
                                        .unwrap_or_default(),
                                }
                            }
                            Err(error) => {
                                search_mode = SearchMode::Bm25Only;
                                self.handle_search_database_error(
                                    error,
                                    "hybrid search",
                                    &mut response_warnings,
                                    &mut extra_warnings,
                                )?
                                .unwrap_or_default()
                            }
                        }
                    }
                    Ok(Err(error)) => {
                        search_mode = SearchMode::Bm25Only;
                        let warning = bm25_fallback_warning_embed_error(
                            &crate::core::config::scrub_sensitive_text(&error.to_string()),
                        );
                        response_warnings.push(warning.clone());
                        extra_warnings.push(SystemWarning {
                            level: "warn".to_string(),
                            message: warning,
                            source: "embed".to_string(),
                        });
                        let query = request.query.clone();
                        let route = route.clone();
                        let scope = scope.clone();
                        let search_options = search_options.clone();
                        match self
                            .run_bm25_search_bounded(query, route, scope, search_options, top_k)
                            .await
                        {
                            Ok(Some(results)) => results,
                            Ok(None) => {
                                push_mcp_timeout_warning(
                                    &mut response_warnings,
                                    &mut extra_warnings,
                                    "BM25 fallback search",
                                    self.search_db_deadline,
                                );
                                Vec::new()
                            }
                            Err(bm25_error) => {
                                let bm25_error = anyhow::anyhow!(
                                    "search failed after vector fallback: {error}; bm25 fallback failed: {bm25_error}"
                                );
                                self.handle_search_database_error(
                                    bm25_error,
                                    "BM25 fallback search",
                                    &mut response_warnings,
                                    &mut extra_warnings,
                                )?
                                .unwrap_or_default()
                            }
                        }
                    }
                    Err(_) => {
                        search_mode = SearchMode::Bm25Only;
                        let warning =
                            bm25_fallback_warning_timeout(config.embed.retry.search_deadline_secs);
                        response_warnings.push(warning.clone());
                        extra_warnings.push(SystemWarning {
                            level: "warn".to_string(),
                            message: warning,
                            source: "embed".to_string(),
                        });
                        let query = request.query.clone();
                        let route = route.clone();
                        let scope = scope.clone();
                        let search_options = search_options.clone();
                        match self
                            .run_bm25_search_bounded(query, route, scope, search_options, top_k)
                            .await
                        {
                            Ok(Some(results)) => results,
                            Ok(None) => {
                                push_mcp_timeout_warning(
                                    &mut response_warnings,
                                    &mut extra_warnings,
                                    "BM25 fallback search",
                                    self.search_db_deadline,
                                );
                                Vec::new()
                            }
                            Err(error) => {
                                let error =
                                    anyhow::anyhow!("search deadline fallback failed: {error}");
                                self.handle_search_database_error(
                                    error,
                                    "BM25 fallback search",
                                    &mut response_warnings,
                                    &mut extra_warnings,
                                )?
                                .unwrap_or_default()
                            }
                        }
                    }
                }
            } else {
                let query = request.query.clone();
                let route = route.clone();
                let scope = scope.clone();
                let search_options = search_options.clone();
                match self
                    .run_bm25_search_bounded(query, route, scope, search_options, top_k)
                    .await
                {
                    Ok(Some(results)) => results,
                    Ok(None) => {
                        push_mcp_timeout_warning(
                            &mut response_warnings,
                            &mut extra_warnings,
                            "BM25 fallback search",
                            self.search_db_deadline,
                        );
                        Vec::new()
                    }
                    Err(error) => {
                        let error =
                            anyhow::anyhow!("search failed after embedder build fallback: {error}");
                        self.handle_search_database_error(
                            error,
                            "BM25 fallback search",
                            &mut response_warnings,
                            &mut extra_warnings,
                        )?
                        .unwrap_or_default()
                    }
                }
            }
        };

        let rerank_outcome = maybe_rerank_search_results(&request.query, results).await;
        for warning in &rerank_outcome.warnings {
            response_warnings.push(warning.clone());
            extra_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: warning.clone(),
                source: "reranker".to_string(),
            });
        }
        let results = rerank_outcome.results;

        // Track hit drawer IDs for session-ingest boost (P13).
        let hit_ids: Vec<String> = results.iter().map(|r| r.drawer_id.clone()).collect();
        if !hit_ids.is_empty() {
            if let Ok(mut guard) = self.session_hit_drawers.lock() {
                guard.extend(hit_ids.iter().cloned());
            }
            dispatch_access_update(self.db_path.clone(), hit_ids);
        }

        let mut system_warnings = current_system_warnings();
        system_warnings.extend(extra_warnings);
        match self
            .run_read_anyhow_bounded(
                |db| Ok(db.vector_index_is_stale().unwrap_or(false)),
                self.search_stale_index_deadline,
            )
            .await
        {
            Ok(Some(vector_index_stale)) => {
                if let Some(warning) = stale_index_warning_from_bool(vector_index_stale) {
                    system_warnings.push(warning);
                }
            }
            Ok(None) => {
                system_warnings.push(SystemWarning {
                    level: "warn".to_string(),
                    message: mcp_stage_timeout_warning(
                        "stale vector index check",
                        self.search_stale_index_deadline,
                    ),
                    source: "mcp_timeout".to_string(),
                });
            }
            Err(error) => {
                let handled = push_mcp_search_database_warning(
                    &mut response_warnings,
                    &mut system_warnings,
                    &self.db_path,
                    "stale vector index check",
                    error.as_ref(),
                );
                if !handled {
                    system_warnings.push(SystemWarning {
                        level: "warn".to_string(),
                        message: format!("stale vector index check failed: {error}"),
                        source: "vector_index".to_string(),
                    });
                }
            }
        }
        if unresolved_scope && config.search.strict_project_isolation {
            system_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: "no project scope resolved, isolation strict".to_string(),
                source: "project_isolation".to_string(),
            });
        }

        Ok(Json(SearchResponse {
            results: results
                .into_iter()
                .map(|result| {
                    SearchResultDto::with_signals_from_result(
                        result,
                        config.search.progressive_disclosure
                            && !request.disable_progressive.unwrap_or(false),
                        config.search.preview_chars,
                    )
                })
                .collect(),
            search_mode: search_mode.as_str().to_string(),
            warnings: response_warnings,
            system_warnings,
        }))
    }

    #[tool(
        name = "mempal_timeline",
        description = "Return a project-scoped narrative overview ordered by importance and recency, without requiring a search query. Prefer this over broad mempal_search when you want project state overview without a specific question in mind."
    )]
    pub async fn mempal_timeline(
        &self,
        Parameters(request): Parameters<TimelineRequest>,
    ) -> std::result::Result<Json<TimelineResponse>, ErrorData> {
        super::timeline::handle(self, request).await
    }

    #[tool(
        name = "mempal_read_drawer",
        description = "Fetch one drawer's full raw verbatim content by drawer_id. Use this after mempal_search returns a truncated preview and you decide the specific drawer is worth reading in full."
    )]
    pub async fn mempal_read_drawer(
        &self,
        Parameters(request): Parameters<ReadDrawerRequest>,
    ) -> std::result::Result<Json<ReadDrawerResponse>, ErrorData> {
        let config = ConfigHandle::current();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let scope = ProjectSearchScope::from_request(
            project_id,
            request.include_global.unwrap_or(false),
            request.all_projects.unwrap_or(false),
            config.search.strict_project_isolation,
        );
        let db = self.open_db()?;
        let details = db
            .get_drawer_details(&request.drawer_id)
            .map_err(db_error)?
            .ok_or_else(|| {
                ErrorData::resource_not_found(
                    "drawer not found",
                    Some(serde_json::json!({
                        "error": "not_found",
                        "drawer_id": request.drawer_id,
                    })),
                )
            })?;
        if !scope.allows_row(details.project_id.as_deref()) {
            return Err(ErrorData::invalid_params(
                format!(
                    "drawer {} is outside the current project scope",
                    request.drawer_id
                ),
                None,
            ));
        }

        Ok(Json(read_drawer_response(details)))
    }

    #[tool(
        name = "mempal_read_drawers",
        description = "Fetch multiple drawers' full raw verbatim content by drawer_id. Returns drawers, not_found ids, and warnings when max_count truncates the batch; use this after mempal_search previews identify a focused subset worth reading in full."
    )]
    pub async fn mempal_read_drawers(
        &self,
        Parameters(request): Parameters<ReadDrawersRequest>,
    ) -> std::result::Result<Json<ReadDrawersResponse>, ErrorData> {
        if request.drawer_ids.len() > MAX_READ_DRAWERS_REQUEST_IDS {
            return Err(ErrorData::invalid_request(
                format!(
                    "drawer_ids exceeds limit: got {}, max {}",
                    request.drawer_ids.len(),
                    MAX_READ_DRAWERS_REQUEST_IDS
                ),
                Some(serde_json::json!({
                    "error": "invalid_request",
                    "field": "drawer_ids",
                    "requested": request.drawer_ids.len(),
                    "max_allowed": MAX_READ_DRAWERS_REQUEST_IDS,
                })),
            ));
        }

        let max_count = request.max_count.unwrap_or(20) as usize;
        if max_count > MAX_READ_DRAWERS_MAX_COUNT {
            return Err(ErrorData::invalid_request(
                format!(
                    "max_count exceeds limit: got {max_count}, max {}",
                    MAX_READ_DRAWERS_MAX_COUNT
                ),
                Some(serde_json::json!({
                    "error": "invalid_request",
                    "field": "max_count",
                    "requested": max_count,
                    "max_allowed": MAX_READ_DRAWERS_MAX_COUNT,
                })),
            ));
        }

        let config = ConfigHandle::current();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let scope = ProjectSearchScope::from_request(
            project_id,
            request.include_global.unwrap_or(false),
            request.all_projects.unwrap_or(false),
            config.search.strict_project_isolation,
        );
        let mut seen = std::collections::HashSet::new();
        let deduped_ids = request
            .drawer_ids
            .into_iter()
            .filter(|drawer_id| seen.insert(drawer_id.clone()))
            .collect::<Vec<_>>();
        let requested_unique_count = deduped_ids.len();
        let requested_ids = if requested_unique_count > max_count {
            deduped_ids[..max_count].to_vec()
        } else {
            deduped_ids
        };
        let db = self.open_db()?;
        let details = db
            .get_drawer_details_batch(&requested_ids)
            .map_err(db_error)?;
        let mut drawers = Vec::with_capacity(details.len());
        let mut found_ids = std::collections::HashSet::new();
        for detail in details {
            let drawer_id = detail.drawer.id.clone();
            if scope.allows_row(detail.project_id.as_deref()) {
                found_ids.insert(drawer_id);
                drawers.push(read_drawer_response(detail));
            }
        }
        let not_found = requested_ids
            .into_iter()
            .filter(|drawer_id| !found_ids.contains(drawer_id))
            .collect();
        let warnings = if requested_unique_count > max_count {
            vec![format!(
                "truncated_to_max_count: requested {requested_unique_count} unique drawer_ids, processed first {max_count} due to max_count={max_count}"
            )]
        } else {
            Vec::new()
        };

        Ok(Json(ReadDrawersResponse {
            drawers,
            not_found,
            warnings,
        }))
    }

    #[tool(
        name = "mempal_context",
        description = "Assemble a mind-model runtime context pack from typed memory. Use the unified `scope` object for project/domain/field context scope; search-only scope fields are rejected instead of ignored. Use this when you need ordered guidance rather than raw search results: dao_tian -> dao_ren -> shu -> qi, with evidence and Phase-2 knowledge cards opt-in. Returns source-backed items with citations and trigger_hints metadata, but never executes skills."
    )]
    async fn mempal_context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> std::result::Result<Json<ContextResponse>, ErrorData> {
        let request_scope = effective_context_scope(&request)?;
        let max_items = request.max_items.unwrap_or(12);
        if max_items == 0 {
            return Err(ErrorData::invalid_params(
                "max_items must be greater than 0",
                None,
            ));
        }
        let dao_tian_limit = request.dao_tian_limit.unwrap_or(1);

        let domain =
            parse_domain(request_scope.domain.as_deref())?.unwrap_or(MemoryDomain::Project);
        let cwd = match request.cwd.as_deref() {
            Some(value) if !value.trim().is_empty() => PathBuf::from(value),
            Some(_) => {
                return Err(ErrorData::invalid_params(
                    "cwd must not be empty when provided",
                    None,
                ));
            }
            None => std::env::current_dir().map_err(|error| {
                ErrorData::internal_error(
                    format!("failed to read current directory: {error}"),
                    None,
                )
            })?,
        };

        let trigger = request.trigger.as_deref().map(parse_context_trigger);
        let config = crate::core::config::ConfigHandle::current();
        let include_cards = request
            .include_cards
            .unwrap_or(config.context.include_cards_default);
        let project_id = if request_scope.all_projects {
            None
        } else {
            self.resolve_mcp_project_id(request_scope.project_id.as_deref(), config.as_ref())
                .await?
        };
        reject_unresolved_strict_context_scope(
            project_id.as_deref(),
            request_scope.all_projects,
            config.as_ref(),
        )?;

        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let query_vector = embedder
            .embed(&[request.query.as_str()])
            .await
            .map_err(|error| ErrorData::internal_error(format!("embedding failed: {error}"), None))?
            .into_iter()
            .next()
            .ok_or_else(|| ErrorData::internal_error("embedder returned no query vector", None))?;

        let db = self.open_db()?;
        let pack = assemble_context_with_vector(
            &db,
            crate::context::ContextRequest {
                query: request.query,
                domain,
                field: request_scope
                    .field
                    .unwrap_or_else(|| anchor::DEFAULT_FIELD.to_string()),
                cwd,
                include_evidence: request.include_evidence.unwrap_or(false),
                include_cards,
                max_items,
                dao_tian_limit,
                project_id,
                trigger,
                context_cfg_override: None,
                include_distill_suggestions: request.include_distill_suggestions.unwrap_or(true),
            },
            &query_vector,
        )
        .map_err(context_error)?;

        Ok(Json(ContextResponse::from(pack)))
    }

    #[tool(
        name = "mempal_knowledge_distill",
        description = "Create candidate knowledge from existing evidence drawer refs. Deterministic Stage-1 distill: writes memory_kind=knowledge/status=candidate for tier dao_ren or qi, validates refs are evidence drawers, and never calls an LLM, promotes, or creates Phase-2 knowledge cards."
    )]
    async fn mempal_knowledge_distill(
        &self,
        Parameters(request): Parameters<KnowledgeDistillRequest>,
    ) -> std::result::Result<Json<KnowledgeDistillResponse>, ErrorData> {
        let dry_run = request.dry_run.unwrap_or(false);
        let core_request = CoreDistillRequest {
            statement: request.statement,
            content: request.content,
            tier: request.tier,
            supporting_refs: request.supporting_refs,
            wing: request.wing.unwrap_or_else(|| "mempal".to_string()),
            room: request.room.unwrap_or_else(|| "knowledge".to_string()),
            domain: request.domain.unwrap_or_else(|| "project".to_string()),
            field: request
                .field
                .unwrap_or_else(|| anchor::DEFAULT_FIELD.to_string()),
            cwd: request.cwd.map(PathBuf::from),
            scope_constraints: request.scope_constraints,
            counterexample_refs: request.counterexample_refs.unwrap_or_default(),
            teaching_refs: request.teaching_refs.unwrap_or_default(),
            trigger_hints: request.trigger_hints.as_ref().map(trigger_hints_from_dto),
            importance: request.importance.unwrap_or(3),
            dry_run,
        };
        let plan = {
            let db = self.open_db()?;
            prepare_distill(&db, core_request).map_err(knowledge_distill_error)?
        };
        let prepared = match plan {
            DistillPlan::Done(outcome) => return Ok(Json(KnowledgeDistillResponse::from(outcome))),
            DistillPlan::Create(prepared) => prepared,
        };

        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let vector = embedder
            .embed(&[prepared.content.as_str()])
            .await
            .map_err(|error| ErrorData::internal_error(format!("embedding failed: {error}"), None))?
            .into_iter()
            .next()
            .ok_or_else(|| ErrorData::internal_error("embedder returned no vector", None))?;
        let db = self.open_db()?;
        let outcome = commit_distill(&db, *prepared, &vector).map_err(knowledge_distill_error)?;
        Ok(Json(KnowledgeDistillResponse::from(outcome)))
    }

    #[tool(
        name = "mempal_knowledge_gate",
        description = "Read-only promotion readiness check for a knowledge drawer. Evaluates whether dao_tian/dao_ren/shu/qi knowledge has enough supporting, verification, teaching, reviewer, and counterexample evidence for the target status. Does not mutate drawers, vectors, schema, audit logs, or lifecycle state."
    )]
    async fn mempal_knowledge_gate(
        &self,
        Parameters(request): Parameters<KnowledgeGateRequest>,
    ) -> std::result::Result<Json<KnowledgeGateResponse>, ErrorData> {
        let db = self.open_db()?;
        let report = evaluate_gate_by_id(
            &db,
            &request.drawer_id,
            request.target_status.as_deref(),
            request.reviewer.as_deref(),
            request.allow_counterexamples.unwrap_or(false),
        )
        .map_err(knowledge_gate_error)?;

        Ok(Json(KnowledgeGateResponse::from(report)))
    }

    #[tool(
        name = "mempal_knowledge_policy",
        description = "Read-only Stage-1 knowledge promotion policy table. Lists deterministic gate thresholds for dao_tian -> canonical, dao_ren -> promoted, shu -> promoted, and qi -> promoted without requiring a drawer and without mutating storage."
    )]
    async fn mempal_knowledge_policy(
        &self,
    ) -> std::result::Result<Json<KnowledgePolicyResponse>, ErrorData> {
        Ok(Json(KnowledgePolicyResponse::from(promotion_policy())))
    }

    #[tool(
        name = "mempal_knowledge_cards",
        description = "Phase-2 knowledge card inspection, linked-evidence retrieval, and governed lifecycle. Actions: list/get/retrieve/events/gate/promote/demote. List supports tier/status/domain/field plus auto_generated and pending_review filters. Retrieve searches linked evidence and returns active cards with citations; promote/demote require evidence refs and append knowledge_events transactionally."
    )]
    async fn mempal_knowledge_cards(
        &self,
        Parameters(request): Parameters<KnowledgeCardsRequest>,
    ) -> std::result::Result<Json<KnowledgeCardsResponse>, ErrorData> {
        let action = trim_to_option(Some(request.action.as_str()))
            .ok_or_else(|| ErrorData::invalid_params("action must not be empty", None))?;

        match action {
            "list" => {
                let db = self.open_db()?;
                let filter = KnowledgeCardFilter {
                    tier: parse_tier(request.tier.as_deref())?,
                    status: parse_status(request.status.as_deref())?,
                    domain: parse_domain(request.domain.as_deref())?,
                    field: trim_to_owned(request.field.as_deref()),
                    anchor_kind: parse_anchor_kind(request.anchor_kind.as_deref())?,
                    anchor_id: trim_to_owned(request.anchor_id.as_deref()),
                    auto_generated: request.auto_generated,
                    pending_review: request.pending_review,
                };
                let cards = db.list_knowledge_cards(&filter).map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to list knowledge cards: {error}"),
                        None,
                    )
                })?;
                Ok(Json(KnowledgeCardsResponse {
                    cards: cards.into_iter().map(KnowledgeCardDto::from).collect(),
                    retrieved: Vec::new(),
                    events: Vec::new(),
                    gate: None,
                    promote: None,
                    demote: None,
                }))
            }
            "get" => {
                let db = self.open_db()?;
                let card_id = required_string(request.card_id.as_deref(), "card_id")?;
                let card = db
                    .get_knowledge_card(card_id)
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("failed to get knowledge card: {error}"),
                            None,
                        )
                    })?
                    .ok_or_else(|| {
                        ErrorData::invalid_params(
                            format!("knowledge card not found: {card_id}"),
                            None,
                        )
                    })?;
                Ok(Json(KnowledgeCardsResponse {
                    cards: vec![KnowledgeCardDto::from(card)],
                    retrieved: Vec::new(),
                    events: Vec::new(),
                    gate: None,
                    promote: None,
                    demote: None,
                }))
            }
            "retrieve" => {
                let query = required_string(request.query.as_deref(), "query")?.to_string();
                let top_k = request.top_k.unwrap_or(5);
                if top_k == 0 {
                    return Err(ErrorData::invalid_params(
                        "top_k must be greater than 0",
                        None,
                    ));
                }
                let domain =
                    parse_domain(request.domain.as_deref())?.unwrap_or(MemoryDomain::Project);
                let field = trim_to_owned(request.field.as_deref())
                    .unwrap_or_else(|| "general".to_string());
                let cwd = request
                    .cwd
                    .as_deref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    });
                let embedder = self.embedder_factory.build().await.map_err(|error| {
                    ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
                })?;
                let query_vector = embedder
                    .embed(&[query.as_str()])
                    .await
                    .map_err(|error| {
                        ErrorData::internal_error(format!("embedding failed: {error}"), None)
                    })?
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        ErrorData::internal_error("embedder returned no query vector", None)
                    })?;
                let db = self.open_db()?;
                let retrieved = retrieve_knowledge_cards_with_vector(
                    &db,
                    CoreCardRetrievalRequest {
                        query,
                        domain,
                        field,
                        cwd,
                        top_k,
                        evidence_top_k: request.evidence_top_k.unwrap_or(top_k * 4),
                    },
                    &query_vector,
                )
                .map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to retrieve knowledge cards: {error}"),
                        None,
                    )
                })?;
                Ok(Json(KnowledgeCardsResponse {
                    cards: Vec::new(),
                    retrieved: retrieved
                        .into_iter()
                        .map(RetrievedKnowledgeCardDto::from)
                        .collect(),
                    events: Vec::new(),
                    gate: None,
                    promote: None,
                    demote: None,
                }))
            }
            "events" => {
                let db = self.open_db()?;
                let card_id = required_string(request.card_id.as_deref(), "card_id")?;
                let events = db.knowledge_events(card_id).map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to list knowledge card events: {error}"),
                        None,
                    )
                })?;
                Ok(Json(KnowledgeCardsResponse {
                    cards: Vec::new(),
                    retrieved: Vec::new(),
                    events: events
                        .into_iter()
                        .map(KnowledgeCardEventDto::from)
                        .collect(),
                    gate: None,
                    promote: None,
                    demote: None,
                }))
            }
            "gate" => {
                let db = self.open_db()?;
                let card_id = required_string(request.card_id.as_deref(), "card_id")?;
                let report = evaluate_card_gate_by_id(
                    &db,
                    card_id,
                    request.target_status.as_deref(),
                    request.reviewer.as_deref(),
                    request.allow_counterexamples.unwrap_or(false),
                )
                .map_err(knowledge_card_lifecycle_error)?;
                Ok(Json(KnowledgeCardsResponse {
                    cards: Vec::new(),
                    retrieved: Vec::new(),
                    events: Vec::new(),
                    gate: Some(report.into()),
                    promote: None,
                    demote: None,
                }))
            }
            "promote" => {
                let db = self.open_db()?;
                let card_id = required_string(request.card_id.as_deref(), "card_id")?;
                let status = required_string(request.status.as_deref(), "status")?.to_string();
                let reason = required_string(request.reason.as_deref(), "reason")?.to_string();
                let verification_refs = request.verification_refs.unwrap_or_default();
                let outcome = promote_card(
                    &db,
                    CorePromoteCardRequest {
                        card_id: card_id.to_string(),
                        status,
                        verification_refs,
                        reason,
                        reviewer: request.reviewer,
                        allow_counterexamples: request.allow_counterexamples.unwrap_or(false),
                        enforce_gate: request.enforce_gate.unwrap_or(true),
                    },
                )
                .map_err(knowledge_card_lifecycle_error)?;
                Ok(Json(KnowledgeCardsResponse {
                    cards: Vec::new(),
                    retrieved: Vec::new(),
                    events: Vec::new(),
                    gate: None,
                    promote: Some(outcome.into()),
                    demote: None,
                }))
            }
            "demote" => {
                let db = self.open_db()?;
                let card_id = required_string(request.card_id.as_deref(), "card_id")?;
                let status = required_string(request.status.as_deref(), "status")?.to_string();
                let reason = required_string(request.reason.as_deref(), "reason")?.to_string();
                let reason_type =
                    required_string(request.reason_type.as_deref(), "reason_type")?.to_string();
                let evidence_refs = request.evidence_refs.unwrap_or_default();
                let outcome = demote_card(
                    &db,
                    CoreDemoteCardRequest {
                        card_id: card_id.to_string(),
                        status,
                        evidence_refs,
                        reason,
                        reason_type,
                    },
                )
                .map_err(knowledge_card_lifecycle_error)?;
                Ok(Json(KnowledgeCardsResponse {
                    cards: Vec::new(),
                    retrieved: Vec::new(),
                    events: Vec::new(),
                    gate: None,
                    promote: None,
                    demote: Some(outcome.into()),
                }))
            }
            other => Err(ErrorData::invalid_params(
                format!(
                    "unsupported knowledge cards action: {other}; actions are list, get, retrieve, events, gate, promote, demote"
                ),
                None,
            )),
        }
    }

    #[tool(
        name = "mempal_knowledge_promote",
        description = "Promote a knowledge drawer after a deterministic gate pass. Appends verification evidence refs, evaluates promotion readiness, then updates lifecycle status and audit log only if the gate allows it."
    )]
    async fn mempal_knowledge_promote(
        &self,
        Parameters(request): Parameters<KnowledgePromoteRequest>,
    ) -> std::result::Result<Json<KnowledgePromoteResponse>, ErrorData> {
        let db = self.open_db()?;
        let outcome = promote_knowledge(
            &db,
            CorePromoteRequest {
                drawer_id: request.drawer_id,
                status: request.status,
                verification_refs: request.verification_refs,
                reason: request.reason,
                reviewer: request.reviewer,
                allow_counterexamples: request.allow_counterexamples.unwrap_or(false),
                enforce_gate: true,
            },
        )
        .map_err(knowledge_lifecycle_error)?;

        Ok(Json(KnowledgePromoteResponse::from(outcome)))
    }

    #[tool(
        name = "mempal_knowledge_demote",
        description = "Demote or retire a knowledge drawer with counterexample evidence. Appends evidence refs to counterexample_refs, updates lifecycle status, and writes an audit entry without touching vectors or schema."
    )]
    async fn mempal_knowledge_demote(
        &self,
        Parameters(request): Parameters<KnowledgeDemoteRequest>,
    ) -> std::result::Result<Json<KnowledgeDemoteResponse>, ErrorData> {
        let db = self.open_db()?;
        let outcome = demote_knowledge(
            &db,
            CoreDemoteRequest {
                drawer_id: request.drawer_id,
                status: request.status,
                evidence_refs: request.evidence_refs,
                reason: request.reason,
                reason_type: request.reason_type,
            },
        )
        .map_err(knowledge_lifecycle_error)?;

        Ok(Json(KnowledgeDemoteResponse::from(outcome)))
    }

    #[tool(
        name = "mempal_knowledge_publish_anchor",
        description = "Publish active knowledge outward across anchor scope. Metadata-only operation for worktree -> repo or repo -> global publication; updates anchor fields and audit log without touching content, vectors, schema, or tier/status lifecycle."
    )]
    async fn mempal_knowledge_publish_anchor(
        &self,
        Parameters(request): Parameters<KnowledgePublishAnchorRequest>,
    ) -> std::result::Result<Json<KnowledgePublishAnchorResponse>, ErrorData> {
        let db = self.open_db()?;
        let outcome = publish_anchor(
            &db,
            CorePublishAnchorRequest {
                drawer_id: request.drawer_id,
                to: request.to,
                target_anchor_id: request.target_anchor_id,
                cwd: request.cwd.map(PathBuf::from),
                reason: request.reason,
                reviewer: request.reviewer,
            },
        )
        .map_err(knowledge_anchor_error)?;

        Ok(Json(KnowledgePublishAnchorResponse::from(outcome)))
    }

    #[tool(
        name = "mempal_ingest",
        description = "Persist a decision, bug fix, design insight, profile fact, or typed knowledge/evidence drawer to project memory. Call this when a durable fact is reached in conversation and include the rationale, not just the outcome. Wing is required; room is optional. Supports typed metadata params (`memory_kind`, `domain`, `field`, `statement`, `tier`, `status`, anchors), pinned facts (`is_pinned`), supersession (`supersedes`/`replace_text`), validity windows, confidence/source_type, dry_run preview, and receipt-based waiting via `wait`/`wait_timeout_secs` (wait=true blocks to a terminal state or returns a timed_out receipt you can poll with `mempal_operation_status`)."
    )]
    pub async fn mempal_ingest(
        &self,
        Parameters(request): Parameters<IngestRequest>,
    ) -> std::result::Result<Json<IngestResponse>, ErrorData> {
        self.mempal_ingest_with_controls(request, IngestControls::default())
            .await
    }

    #[doc(hidden)]
    pub async fn mempal_ingest_with_controls(
        &self,
        request: IngestRequest,
        controls: IngestControls,
    ) -> std::result::Result<Json<IngestResponse>, ErrorData> {
        let dry_run = request.dry_run.unwrap_or(false);
        if !dry_run && global_embed_status().should_block_writes() {
            return Err(degraded_write_error());
        }
        if !dry_run && let Err(error) = self.async_db().await {
            return Err(database_write_refused_error(
                &self.db_path,
                "async_db",
                error.as_ref(),
            ));
        }
        self.spawn_ingest_drain_worker();
        let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
        let room = request.room.as_deref();
        // Snapshot the request-wide warnings once so every early-return path reports a
        // consistent set from a single `sqlite_master` read. A mid-request embed transition
        // to degraded is not lost: the degraded-write guards above reject before any
        // success response that would carry this snapshot is built.
        let request_system_warnings = self
            .ingest_system_warnings_with_stale_index_bounded(self.ingest_admission_deadline)
            .await?;
        let wait = request.wait.unwrap_or(false);
        let wait_timeout_secs = request.wait_timeout_secs.unwrap_or(30);
        let raw_turn = is_raw_turn(&request.wing, room, &config.turns);
        if raw_turn && !should_store_raw_turns(&config.turns.storage_mode) {
            return Ok(Json(IngestResponse {
                operation_id: None,
                accepted_at: None,
                state: None,
                timed_out: false,
                drawer_id: String::new(),
                drawer_ids: Vec::new(),
                chunk_count: 0,
                dropped: false,
                gating_decision: None,
                novelty_action: None,
                near_drawer_id: None,
                duplicate_warning: None,
                lock_wait_ms: None,
                superseded_drawer_id: None,
                rejected_reason: None,
                failure_detail: None,
                timings: BTreeMap::new(),
                fact_check_warnings: Vec::new(),
                system_warnings: request_system_warnings,
            }));
        }
        if dry_run {
            let response = match tokio::time::timeout(
                self.ingest_admission_deadline,
                self.run_prepared_ingest_off_runtime(request, controls),
            )
            .await
            {
                Ok(Ok(Ok(response))) => response,
                Ok(Ok(Err(error))) => return Err(error),
                Ok(Err(error)) => {
                    return Err(ErrorData::internal_error(
                        format!("failed to run dry-run ingest off runtime: {error}"),
                        None,
                    ));
                }
                Err(_) => {
                    return Err(mcp_stage_timeout_error(
                        "mempal_ingest",
                        "dry-run admission",
                        self.ingest_admission_deadline,
                    ));
                }
            };
            return Ok(Json(response));
        }
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let prepared = match tokio::time::timeout(
            self.ingest_admission_deadline,
            self.prepare_async_ingest_operation(
                &request,
                controls,
                config.as_ref(),
                compiled_privacy.as_ref(),
                project_id,
            ),
        )
        .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(mcp_stage_timeout_error(
                    "mempal_ingest",
                    "admission preparation",
                    self.ingest_admission_deadline,
                ));
            }
        };
        let payload = serde_json::to_string(&prepared).map_err(|error| {
            ErrorData::internal_error(format!("failed to serialize ingest request: {error}"), None)
        })?;
        let operation_id = match self.enqueue_ingest_operation(payload).await {
            Ok(operation_id) => operation_id,
            Err(error) => {
                if let Some(error) = maybe_database_write_refused_error(
                    &self.db_path,
                    "enqueue_ingest_operation",
                    error.as_ref(),
                ) {
                    return Err(error);
                }
                return Err(ErrorData::internal_error(
                    format!("failed to enqueue ingest operation: {error}"),
                    None,
                ));
            }
        };

        let queued_response = IngestResponse {
            operation_id: Some(operation_id),
            accepted_at: Some(crate::core::utils::iso_timestamp()),
            state: Some(IngestOperationState::Queued),
            timed_out: false,
            drawer_id: String::new(),
            drawer_ids: Vec::new(),
            chunk_count: 0,
            dropped: false,
            gating_decision: None,
            novelty_action: None,
            near_drawer_id: None,
            duplicate_warning: None,
            lock_wait_ms: None,
            superseded_drawer_id: None,
            rejected_reason: None,
            failure_detail: None,
            timings: BTreeMap::new(),
            fact_check_warnings: Vec::new(),
            system_warnings: request_system_warnings,
        };

        if wait {
            if let Some(final_response) = self
                .wait_for_operation_status(
                    queued_response
                        .operation_id
                        .as_deref()
                        .expect("queued receipt must include operation id"),
                    Duration::from_secs(wait_timeout_secs),
                    Duration::from_millis(150),
                )
                .await?
            {
                return Ok(Json(final_response));
            }

            let mut timed_out_response = queued_response;
            timed_out_response.timed_out = true;
            return Ok(Json(timed_out_response));
        }

        Ok(Json(queued_response))
    }

    async fn enqueue_ingest_operation(&self, payload: String) -> anyhow::Result<String> {
        let idempotency_key = mcp_ingest_idempotency_key(&payload);
        if let Some(operation_id) = self
            .try_enqueue_ingest_operation_via_daemon(payload.clone(), idempotency_key.clone())
            .await?
        {
            return Ok(operation_id);
        }

        self.async_queue
            .enqueue_idempotent_with_key(INGEST_ASYNC_KIND.to_string(), payload, idempotency_key)
            .await
            .map_err(Into::into)
    }

    async fn try_enqueue_ingest_operation_via_daemon(
        &self,
        payload: String,
        idempotency_key: String,
    ) -> anyhow::Result<Option<String>> {
        let Some(mempal_home) = self.db_path.parent().map(Path::to_path_buf) else {
            return Ok(None);
        };
        let operation_id =
            PendingMessageStore::idempotent_message_id(INGEST_ASYNC_KIND, &idempotency_key);
        let request = crate::hook_ipc::HookIpcEnqueueRequest {
            kind: INGEST_ASYNC_KIND.to_string(),
            payload,
            idempotency_key,
        };
        let outcome = tokio::task::spawn_blocking(move || {
            crate::hook_ipc::enqueue_with_default_timeout(&mempal_home, request)
        })
        .await
        .context("blocking daemon ingest enqueue IPC failed")?;

        match outcome {
            crate::hook_ipc::HookIpcClientOutcome::Accepted => Ok(Some(operation_id)),
            crate::hook_ipc::HookIpcClientOutcome::Fallback(reason) => {
                tracing::debug!(reason = %reason, "daemon ingest enqueue unavailable; using local queue");
                Ok(None)
            }
        }
    }

    async fn prepare_async_ingest_operation(
        &self,
        request: &IngestRequest,
        controls: IngestControls,
        config: &crate::core::config::Config,
        compiled_privacy: &crate::core::config::CompiledPrivacyConfig,
        project_id: Option<String>,
    ) -> std::result::Result<PreparedIngestOperation, ErrorData> {
        let scrubbed_content =
            config.scrub_content_with_compiled(&request.content, compiled_privacy);
        let scrubbed_source = request
            .source
            .as_deref()
            .map(|value| config.scrub_content_with_compiled(value, compiled_privacy));
        let scrubbed_source_file = request
            .source_file
            .as_deref()
            .map(|value| config.scrub_content_with_compiled(value, compiled_privacy));
        let room = request.room.as_deref();
        let raw_turn = is_raw_turn(&request.wing, room, &config.turns);
        let drawer_importance = raw_turn_importance(&request.wing, room, &config.turns)
            .unwrap_or_else(|| request.importance.unwrap_or(0));
        let source_type = parse_source_type_param(request.source_type.as_deref())?;
        let confidence = resolve_confidence_param(source_type, request.confidence)?;
        let metadata = validate_ingest_request(request, &source_type)?;
        let scrubbed_replace_text = request
            .replace_text
            .as_deref()
            .map(|text| config.scrub_content_with_compiled(text, compiled_privacy));
        let replacement_target = self
            .resolve_replacement_target_async(
                request.supersedes.clone(),
                scrubbed_replace_text.clone(),
                request.wing.clone(),
                request.room.clone(),
                project_id.clone(),
            )
            .await?;
        let mut request = request.clone();
        request.content = scrubbed_content.clone();
        request.source = scrubbed_source;
        request.source_file = scrubbed_source_file;
        request.replace_text = scrubbed_replace_text;

        Ok(PreparedIngestOperation {
            request,
            controls,
            project_id,
            scrubbed_content,
            source_type,
            confidence,
            metadata,
            superseded_drawer_id: replacement_target.map(|summary| summary.id),
            raw_turn,
            drawer_importance,
        })
    }

    async fn resolve_replacement_target_async(
        &self,
        supersedes: Option<String>,
        replace_text: Option<String>,
        wing: String,
        room: Option<String>,
        project_id: Option<String>,
    ) -> std::result::Result<Option<DrawerSummary>, ErrorData> {
        let async_db = self.async_db().await.map_err(db_error)?;
        async_db
            .run_read(move |db| {
                db.resolve_replacement_target(
                    supersedes.as_deref(),
                    replace_text.as_deref(),
                    &wing,
                    room.as_deref(),
                    project_id.as_deref(),
                )
            })
            .await
            .map_err(replacement_db_error)
    }

    async fn mempal_ingest_sync(
        &self,
        request: IngestRequest,
        controls: IngestControls,
    ) -> std::result::Result<Json<IngestResponse>, ErrorData> {
        let dry_run = request.dry_run.unwrap_or(false);
        if !dry_run && global_embed_status().should_block_writes() {
            return Err(degraded_write_error());
        }
        let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let scrubbed_content =
            config.scrub_content_with_compiled(&request.content, compiled_privacy.as_ref());
        let room = request.room.as_deref();
        let db = Database::open(&self.db_path).map_err(|error| {
            database_write_refused_error(&self.db_path, "sync_ingest_open_db", &error)
        })?;
        // Snapshot the request-wide warnings once so every early-return path reports a
        // consistent set from a single `sqlite_master` read. A mid-request embed transition
        // to degraded is not lost: the degraded-write guard above rejects before any
        // success response that would carry this snapshot is built.
        let request_system_warnings = system_warnings_with_stale_index(&db);
        let no_gate = controls.no_gate;
        let bypass_novelty = controls.bypass_novelty;
        let raw_turn = is_raw_turn(&request.wing, room, &config.turns);
        if raw_turn && !should_store_raw_turns(&config.turns.storage_mode) {
            return Ok(Json(IngestResponse {
                operation_id: None,
                accepted_at: None,
                state: None,
                timed_out: false,
                drawer_id: String::new(),
                drawer_ids: Vec::new(),
                chunk_count: 0,
                dropped: false,
                gating_decision: None,
                novelty_action: None,
                near_drawer_id: None,
                duplicate_warning: None,
                lock_wait_ms: None,
                superseded_drawer_id: None,
                rejected_reason: None,
                failure_detail: None,
                timings: BTreeMap::new(),
                fact_check_warnings: Vec::new(),
                system_warnings: request_system_warnings,
            }));
        }
        let drawer_importance = raw_turn_importance(&request.wing, room, &config.turns)
            .unwrap_or_else(|| request.importance.unwrap_or(0));
        let source_type = parse_source_type_param(request.source_type.as_deref())?;
        let confidence = resolve_confidence_param(source_type, request.confidence)?;
        let metadata = validate_ingest_request(&request, &source_type)?;
        let mut timings = BTreeMap::new();

        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let chunks =
            crate::ingest::prepare_chunks(&scrubbed_content, &config.chunker, embedder.as_ref());
        if chunks.is_empty() {
            return Err(ErrorData::invalid_params(
                "content produced no chunks",
                None,
            ));
        }

        let scrubbed_replace_text = request
            .replace_text
            .as_deref()
            .map(|text| config.scrub_content_with_compiled(text, compiled_privacy.as_ref()));
        let replacement_target = db
            .resolve_replacement_target(
                request.supersedes.as_deref(),
                scrubbed_replace_text.as_deref(),
                &request.wing,
                room,
                project_id.as_deref(),
            )
            .map_err(replacement_db_error)?;
        let superseded_drawer_id = replacement_target
            .as_ref()
            .map(|summary| summary.id.clone());
        let superseded_drawer_id_ref = superseded_drawer_id.as_deref();
        let mut superseded_response_id: Option<String> = None;

        let mut chunk_drawer_ids: Vec<(usize, String, bool)> = Vec::with_capacity(chunks.len());
        for (idx, chunk) in chunks.iter().enumerate() {
            if let Some(existing_id) = exact_duplicate_drawer_id(
                &db,
                chunk,
                &request.wing,
                room,
                project_id.as_deref(),
                superseded_drawer_id_ref,
                &metadata,
            )? {
                chunk_drawer_ids.push((idx, existing_id, true));
                continue;
            }

            let preferred_id = build_bootstrap_drawer_id_from_parts(
                &request.wing,
                room,
                chunk,
                metadata.identity_parts(),
            );
            let did = db
                .resolve_available_drawer_id(&preferred_id)
                .map_err(db_error)?;
            let exists = db.drawer_exists(&did).map_err(db_error)?;
            chunk_drawer_ids.push((idx, did, exists));
        }
        let drawer_id = chunk_drawer_ids
            .first()
            .map(|(_, id, _)| id.clone())
            .unwrap_or_default();

        if dry_run {
            let all_ids: Vec<String> = chunk_drawer_ids
                .iter()
                .map(|(_, id, _)| id.clone())
                .collect();
            return Ok(Json(IngestResponse {
                drawer_id,
                drawer_ids: all_ids,
                chunk_count: chunks.len(),
                dropped: false,
                gating_decision: None,
                novelty_action: None,
                near_drawer_id: None,
                duplicate_warning: None,
                lock_wait_ms: None,
                superseded_drawer_id,
                fact_check_warnings: Vec::new(),
                system_warnings: request_system_warnings.clone(),
                ..Default::default()
            }));
        }

        if chunk_drawer_ids.iter().all(|(_, _, exists)| *exists) {
            let all_ids = chunk_drawer_ids
                .iter()
                .map(|(_, id, _)| id.clone())
                .collect::<Vec<_>>();
            if metadata.is_pinned {
                for id in &all_ids {
                    db.pin_drawer(id, None).map_err(db_error)?;
                }
            }
            if let Some(old_id) = superseded_drawer_id.as_deref() {
                let replacement_id = all_ids.first().map(String::as_str).unwrap_or("existing");
                supersede_drawer_for_ingest(&db, old_id, replacement_id)?;
                superseded_response_id = Some(old_id.to_string());
            }
            return Ok(Json(IngestResponse {
                drawer_id,
                drawer_ids: all_ids,
                chunk_count: chunks.len(),
                dropped: false,
                gating_decision: None,
                novelty_action: None,
                near_drawer_id: None,
                duplicate_warning: None,
                lock_wait_ms: None,
                superseded_drawer_id: superseded_response_id,
                fact_check_warnings: Vec::new(),
                system_warnings: request_system_warnings.clone(),
                ..Default::default()
            }));
        }

        let gating_started = Instant::now();
        let candidate = IngestCandidate {
            content: scrubbed_content.clone(),
            event: None,
            tool_name: None,
            exit_code: None,
        };
        let mut gating_decision: Option<GatingDecision> = None;
        let mut fact_check_warnings = Vec::new();
        let mut should_enqueue_llm_task = false;
        let mut first_vector = None;
        if !no_gate {
            let mut gating_audit_recorded = false;
            gating_decision = evaluate_tier1(&candidate, &config.ingest_gating);
            if let Some(decision) = gating_decision.as_ref()
                && decision.is_rejected()
            {
                db.record_gating_audit(
                    &drawer_id,
                    decision,
                    project_id.as_deref(),
                    Some(&candidate.content),
                )
                .map_err(db_error)?;
                return Ok(Json(IngestResponse {
                    drawer_id,
                    drawer_ids: Vec::new(),
                    chunk_count: 0,
                    dropped: true,
                    gating_decision,
                    novelty_action: None,
                    near_drawer_id: None,
                    duplicate_warning: None,
                    lock_wait_ms: None,
                    superseded_drawer_id: None,
                    fact_check_warnings: Vec::new(),
                    system_warnings: request_system_warnings.clone(),
                    ..Default::default()
                }));
            }

            if gating_decision.is_none() {
                let tier2_classifier = if config.ingest_gating.enabled
                    && config.ingest_gating.embedding_classifier.enabled
                {
                    self.gating_runtime
                        .classifier()
                        .await
                        .map_err(|error| ErrorData::internal_error(error.to_string(), None))?
                } else {
                    None
                };
                if let Some(classifier) = tier2_classifier.as_ref() {
                    let tier2 = evaluate_tier2(
                        &candidate,
                        classifier,
                        embedder.as_ref(),
                        config.ingest_gating.embedding_classifier.threshold,
                    )
                    .await;
                    first_vector = tier2.vector;
                    // Tier 3: route calibrated candidates to LLM judge when enabled
                    // (fail-open: store now, judge async). The routing helper preserves
                    // mechanical Tier 1 skips while allowing quality policies such as
                    // llm_first to judge Tier 2 keeps.
                    if superseded_drawer_id.is_none()
                        && should_apply_async_llm_gating(source_type)
                        && should_route_to_llm_judge(&config, &Some(tier2.decision.clone()))
                    {
                        let llm_decision =
                            GatingDecision::accepted(0, Some("llm_pending".to_string()), None);
                        db.record_gating_audit(
                            &drawer_id,
                            &llm_decision,
                            project_id.as_deref(),
                            Some(&candidate.content),
                        )
                        .map_err(db_error)?;
                        gating_audit_recorded = true;
                        gating_decision = Some(llm_decision);
                        should_enqueue_llm_task = true;
                    } else {
                        db.record_gating_audit(
                            &drawer_id,
                            &tier2.decision,
                            project_id.as_deref(),
                            Some(&candidate.content),
                        )
                        .map_err(db_error)?;
                        gating_audit_recorded = true;
                        gating_decision = Some(tier2.decision);
                    }
                } else if config.ingest_gating.enabled {
                    gating_decision = Some(GatingDecision::accepted(
                        0,
                        Some("tier2_disabled".to_string()),
                        None,
                    ));
                }
            }

            if let Some(decision) = gating_decision.as_ref()
                && decision.is_rejected()
            {
                return Ok(Json(IngestResponse {
                    drawer_id,
                    drawer_ids: Vec::new(),
                    chunk_count: 0,
                    dropped: true,
                    gating_decision,
                    novelty_action: None,
                    near_drawer_id: None,
                    duplicate_warning: None,
                    lock_wait_ms: None,
                    superseded_drawer_id: None,
                    fact_check_warnings: Vec::new(),
                    system_warnings: request_system_warnings.clone(),
                    ..Default::default()
                }));
            }
            if !gating_audit_recorded && let Some(decision) = gating_decision.as_ref() {
                db.record_gating_audit(
                    &drawer_id,
                    decision,
                    project_id.as_deref(),
                    Some(&candidate.content),
                )
                .map_err(db_error)?;
            }
        }

        let mut db = db;

        let mempal_home = db
            .path()
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        let lock_guard = crate::ingest::lock::acquire_source_lock(
            &mempal_home,
            &drawer_id,
            std::time::Duration::from_secs(5),
        )
        .map_err(|e| ErrorData::internal_error(format!("ingest lock: {e}"), None))?;
        let lock_wait_ms = Some(lock_guard.wait_duration().as_millis() as u64);

        let first_chunk = chunks.first().map(|c| c.as_str()).unwrap_or("");
        if !no_gate
            && !raw_turn
            && let Some(outcome) = evaluate_fact_check_gate(
                &drawer_id,
                &candidate.content,
                &db,
                project_id.as_deref(),
                &config.ingest_gating.fact_check,
                confidence,
            )
            .map_err(db_error)?
        {
            fact_check_warnings = outcome.warnings;
            gating_decision = Some(outcome.decision);
            if gating_decision
                .as_ref()
                .is_some_and(GatingDecision::is_rejected)
            {
                drop(lock_guard);
                return Ok(Json(IngestResponse {
                    drawer_id,
                    drawer_ids: Vec::new(),
                    chunk_count: 0,
                    dropped: true,
                    gating_decision,
                    novelty_action: None,
                    near_drawer_id: None,
                    duplicate_warning: None,
                    lock_wait_ms,
                    superseded_drawer_id: None,
                    fact_check_warnings,
                    system_warnings: request_system_warnings.clone(),
                    ..Default::default()
                }));
            }
        }
        timings.insert(
            "gating_ms".to_string(),
            gating_started.elapsed().as_millis() as u64,
        );

        let embedding_started = Instant::now();
        let chunk_refs: Vec<&str> = chunks.iter().map(|c| c.as_str()).collect();
        let vectors = if first_vector.is_some() && chunks.len() == 1 {
            vec![first_vector.take().expect("checked Some")]
        } else if let Some(fv) = first_vector.take() {
            if chunks.len() > 1 {
                let rest_refs: Vec<&str> = chunk_refs[1..].to_vec();
                let mut rest_vecs = embedder.embed(&rest_refs).await.map_err(|error| {
                    ErrorData::internal_error(format!("embedding failed: {error}"), None)
                })?;
                let mut all = vec![fv];
                all.append(&mut rest_vecs);
                all
            } else {
                vec![fv]
            }
        } else {
            embedder.embed(&chunk_refs).await.map_err(|error| {
                ErrorData::internal_error(format!("embedding failed: {error}"), None)
            })?
        };
        if vectors.len() != chunks.len() {
            return Err(ErrorData::internal_error(
                format!(
                    "embedder returned {} vectors for {} chunks",
                    vectors.len(),
                    chunks.len()
                ),
                None,
            ));
        }
        if let Some(v) = vectors.first() {
            ensure_vector_dim_matches(&db, v.len())?;
        }
        timings.insert(
            "embedding_ms".to_string(),
            embedding_started.elapsed().as_millis() as u64,
        );

        let first_vector_ref = &vectors[0];
        let novelty_started = Instant::now();
        let duplicate_warning = check_semantic_duplicate(&db, first_vector_ref, first_chunk);
        let novelty_candidate = NoveltyCandidate {
            wing: request.wing.clone(),
            room: request.room.clone(),
            project_id: project_id.clone(),
        };
        let novelty = if superseded_drawer_id.is_some() || bypass_novelty {
            crate::ingest::novelty::NoveltyDecision {
                should_audit: false,
                ..crate::ingest::novelty::NoveltyDecision::insert()
            }
        } else {
            evaluate_novelty(
                &db,
                &novelty_candidate,
                first_vector_ref,
                &config.ingest_gating.novelty,
            )
        };
        timings.insert(
            "novelty_ms".to_string(),
            novelty_started.elapsed().as_millis() as u64,
        );
        let mut response_drawer_id = drawer_id.clone();
        let (novelty_action, near_drawer_id);

        let mut inserted_drawer_ids: Vec<String> = Vec::new();
        // Tracks only drawers freshly created in this request — dedup-resolved IDs (pre-existing
        // drawers found by hash) must NOT appear here, so LLM reject cannot soft-delete them.
        let mut newly_created_drawer_ids: Vec<String> = Vec::new();

        let db_write_started = Instant::now();
        match novelty.action {
            NoveltyAction::Insert => {
                if novelty.should_audit {
                    db.record_novelty_audit(
                        &drawer_id,
                        NoveltyAction::Insert,
                        novelty.near_drawer_id.as_deref(),
                        novelty.cosine,
                        novelty.audit_decision,
                        project_id.as_deref(),
                    )
                    .map_err(db_error)?;
                }
                novelty_action = Some(NoveltyAction::Insert);
                near_drawer_id = novelty.near_drawer_id.clone();

                for ((chunk_idx, chunk_did, chunk_exists), (chunk, vector)) in chunk_drawer_ids
                    .iter()
                    .zip(chunks.iter().zip(vectors.iter()))
                {
                    if *chunk_exists {
                        // Dedup-resolved pre-lock: include in response but NOT in
                        // newly_created_drawer_ids so LLM reject cannot delete it.
                        if metadata.is_pinned {
                            db.pin_drawer(chunk_did, None).map_err(db_error)?;
                        }
                        inserted_drawer_ids.push(chunk_did.clone());
                        continue;
                    }
                    let _extra_lock = if *chunk_idx > 0 {
                        Some(
                            crate::ingest::lock::acquire_source_lock(
                                &mempal_home,
                                chunk_did,
                                std::time::Duration::from_secs(5),
                            )
                            .map_err(|e| {
                                ErrorData::internal_error(
                                    format!("ingest lock chunk {chunk_idx}: {e}"),
                                    None,
                                )
                            })?,
                        )
                    } else {
                        None
                    };
                    let exists_after_lock = db.drawer_exists(chunk_did).map_err(db_error)?;
                    if exists_after_lock {
                        // Dedup-resolved post-lock: include in response but NOT in
                        // newly_created_drawer_ids so LLM reject cannot delete it.
                        if metadata.is_pinned {
                            db.pin_drawer(chunk_did, None).map_err(db_error)?;
                        }
                        inserted_drawer_ids.push(chunk_did.clone());
                        continue;
                    }
                    let mut drawer = drawer_from_ingest_metadata(
                        &request,
                        &metadata,
                        chunk_did,
                        chunk,
                        *chunk_idx,
                        SourceConfidence {
                            source_type,
                            confidence,
                        },
                        drawer_importance,
                    );
                    if let Some(old_id) = superseded_drawer_id.as_deref() {
                        link_superseded_drawer(&mut drawer, old_id);
                    }
                    db.insert_drawer_with_project_validity(
                        &drawer,
                        project_id.as_deref(),
                        None,
                        request.valid_from.as_deref(),
                        request.valid_until.as_deref(),
                    )
                    .map_err(db_error)?;
                    db.insert_vector_with_project(chunk_did, vector, project_id.as_deref())
                        .map_err(db_error)?;
                    inserted_drawer_ids.push(chunk_did.clone());
                    newly_created_drawer_ids.push(chunk_did.clone());
                }
            }
            NoveltyAction::Drop => {
                if novelty.should_audit {
                    db.record_novelty_audit(
                        &drawer_id,
                        NoveltyAction::Drop,
                        novelty.near_drawer_id.as_deref(),
                        novelty.cosine,
                        novelty.audit_decision,
                        project_id.as_deref(),
                    )
                    .map_err(db_error)?;
                }
                novelty_action = Some(NoveltyAction::Drop);
                near_drawer_id = novelty.near_drawer_id.clone();
                response_drawer_id = novelty.near_drawer_id.unwrap_or(drawer_id.clone());
            }
            NoveltyAction::Merge => {
                let target_id = novelty.near_drawer_id.clone().ok_or_else(|| {
                    ErrorData::internal_error("novelty merge missing target", None)
                })?;
                let _target_lock = if target_id == drawer_id {
                    None
                } else {
                    Some(
                        crate::ingest::lock::acquire_source_lock(
                            &mempal_home,
                            &target_id,
                            std::time::Duration::from_secs(5),
                        )
                        .map_err(|e| {
                            ErrorData::internal_error(format!("merge target lock: {e}"), None)
                        })?,
                    )
                };
                let (existing_content, merge_count) = db
                    .drawer_merge_state(&target_id)
                    .map_err(db_error)?
                    .ok_or_else(|| {
                        ErrorData::internal_error("novelty merge target missing", None)
                    })?;
                let merged_at = current_timestamp();
                let merged_content = format!(
                    "{existing_content}\n---\nSUPPLEMENTARY ({merged_at}):\n{scrubbed_content}"
                );
                let capped = merge_count >= config.ingest_gating.novelty.max_merges_per_drawer
                    || merged_content.len()
                        > config.ingest_gating.novelty.max_content_bytes_per_drawer;
                if capped {
                    self.mcp_ingest_insert_fallback(
                        &mut db,
                        &drawer_id,
                        &scrubbed_content,
                        &request,
                        &chunks,
                        &vectors,
                        &chunk_drawer_ids,
                        &mempal_home,
                        project_id.as_deref(),
                        &target_id,
                        &novelty,
                        Some("insert_due_to_merge_cap"),
                        drawer_importance,
                        source_type,
                        confidence,
                        &mut inserted_drawer_ids,
                        &mut newly_created_drawer_ids,
                    )?;
                    novelty_action = Some(NoveltyAction::Insert);
                    near_drawer_id = Some(target_id);
                } else {
                    match embedder.embed(&[merged_content.as_str()]).await {
                        Ok(merged_vectors) => match merged_vectors.into_iter().next() {
                            Some(merged_vector) => {
                                ensure_vector_dim_matches(&db, merged_vector.len())?;
                                db.update_drawer_after_merge_and_record_novelty_audit(
                                    &target_id,
                                    &merged_content,
                                    &merged_at,
                                    &merged_vector,
                                    merge_count,
                                    NoveltyAuditInsert {
                                        candidate_hash: &drawer_id,
                                        action: NoveltyAction::Merge,
                                        near_drawer_id: Some(target_id.as_str()),
                                        cosine: novelty.cosine,
                                        audit_decision: novelty.audit_decision,
                                        project_id: project_id.as_deref(),
                                    },
                                )
                                .map_err(db_error)?;
                                novelty_action = Some(NoveltyAction::Merge);
                                near_drawer_id = Some(target_id.clone());
                                response_drawer_id = target_id;
                            }
                            None => {
                                tracing::warn!(
                                    target_id = %target_id,
                                    candidate_drawer_id = %drawer_id,
                                    merged_content_bytes = merged_content.len(),
                                    "novelty merge re-embed returned no vector; fail-open insert"
                                );
                                self.mcp_ingest_insert_fallback(
                                    &mut db,
                                    &drawer_id,
                                    &scrubbed_content,
                                    &request,
                                    &chunks,
                                    &vectors,
                                    &chunk_drawer_ids,
                                    &mempal_home,
                                    project_id.as_deref(),
                                    &target_id,
                                    &novelty,
                                    Some("insert_due_to_embed_error"),
                                    drawer_importance,
                                    source_type,
                                    confidence,
                                    &mut inserted_drawer_ids,
                                    &mut newly_created_drawer_ids,
                                )?;
                                novelty_action = Some(NoveltyAction::Insert);
                                near_drawer_id = Some(target_id);
                            }
                        },
                        Err(_error) => {
                            tracing::warn!(
                                candidate_drawer_id = %drawer_id,
                                "novelty merge re-embed failed; fail-open insert"
                            );
                            self.mcp_ingest_insert_fallback(
                                &mut db,
                                &drawer_id,
                                &scrubbed_content,
                                &request,
                                &chunks,
                                &vectors,
                                &chunk_drawer_ids,
                                &mempal_home,
                                project_id.as_deref(),
                                &target_id,
                                &novelty,
                                Some("insert_due_to_embed_error"),
                                drawer_importance,
                                source_type,
                                confidence,
                                &mut inserted_drawer_ids,
                                &mut newly_created_drawer_ids,
                            )?;
                            novelty_action = Some(NoveltyAction::Insert);
                            near_drawer_id = Some(target_id);
                        }
                    }
                }
            }
        }

        timings.insert(
            "db_write_ms".to_string(),
            db_write_started.elapsed().as_millis() as u64,
        );

        if let Some(old_id) = superseded_drawer_id.as_deref()
            && let Some(replacement_id) = inserted_drawer_ids.first()
        {
            supersede_drawer_for_ingest(&db, old_id, replacement_id)?;
            superseded_response_id = Some(old_id.to_string());
        }

        drop(lock_guard);

        // Apply session-ingest boost to previously searched drawers (P13).
        {
            let hit_ids: Vec<String> = self
                .session_hit_drawers
                .lock()
                .map(|mut guard| {
                    let ids: Vec<String> = guard.iter().cloned().collect();
                    guard.clear();
                    ids
                })
                .unwrap_or_default();
            if !hit_ids.is_empty() {
                use std::time::{SystemTime, UNIX_EPOCH};
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let imp = &config.importance;
                let db_boost = self.open_db()?;
                if let Err(err) = db_boost.apply_ingest_boost_batch(
                    &hit_ids,
                    now_ms,
                    imp.boost_per_access,
                    imp.boost_cap,
                    imp.decay_rate,
                    imp.floor,
                ) {
                    tracing::warn!(error = %err, "session-ingest boost failed");
                }
            }
        }

        if !inserted_drawer_ids.is_empty() {
            response_drawer_id = inserted_drawer_ids[0].clone();
        }

        // Tier 3 LLM judge (P12) — fire-and-forget after drawer is stored.
        // Only runs when Tier 2 returned "prototype_below_threshold" and LLM judge is active.
        // Guard: only enqueue for NEWLY CREATED drawers. Dedup-resolved IDs (pre-existing drawers
        // found by hash) are excluded from newly_created_drawer_ids so a subsequent LLM reject
        // cannot soft-delete a drawer that predated this ingest request.
        if should_enqueue_llm_task && !newly_created_drawer_ids.is_empty() {
            let system_prompt = config
                .ingest_gating
                .llm_judge
                .as_ref()
                .and_then(|j| j.system_prompt.clone());
            let payload = crate::llm::LlmTaskPayload {
                task_type: "gating".to_string(),
                drawer_id: newly_created_drawer_ids[0].clone(),
                drawer_ids: newly_created_drawer_ids.clone(),
                content: scrubbed_content.clone(),
                system_prompt,
            };
            match serde_json::to_string(&payload) {
                Ok(payload_json) => match crate::core::queue::PendingMessageStore::new(db.path()) {
                    Ok(queue) => {
                        if let Err(err) = queue.enqueue("llm_task", &payload_json) {
                            tracing::warn!(
                                error = %err,
                                drawer_ids = ?newly_created_drawer_ids,
                                "Tier 3 LLM gating task enqueue failed; fail-open keep"
                            );
                        }
                    }
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "Tier 3 LLM gating queue init failed; fail-open keep"
                        );
                    }
                },
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "Tier 3 LLM gating payload serialization failed; fail-open keep"
                    );
                }
            }
        }

        // Failure detection (P14) — fire-and-forget for each inserted drawer.
        if config.repair.enabled && !inserted_drawer_ids.is_empty() {
            for (drawer_id_r, chunk_r) in inserted_drawer_ids.iter().zip(chunks.iter()) {
                crate::repair::spawn_failure_detection(
                    db.path().to_path_buf(),
                    drawer_id_r.clone(),
                    chunk_r.to_string(),
                    request.wing.clone(),
                    room.map(ToOwned::to_owned),
                    project_id.clone(),
                    config.repair.clone(),
                );
            }
        }

        // Pattern detection (P13) — fire-and-forget for each inserted drawer.
        if config.patterns.enabled && !inserted_drawer_ids.is_empty() {
            let session_id = request
                .source
                .as_deref()
                .unwrap_or_else(|| inserted_drawer_ids[0].as_str());
            let model_id = config.embed.model.clone().unwrap_or_else(|| {
                if config.embed.backend == "model2vec" {
                    "model2vec/potion-multilingual-128M".to_string()
                } else {
                    config.embed.model.clone().unwrap_or_default()
                }
            });
            for (drawer_id_p, vector_p) in inserted_drawer_ids.iter().zip(vectors.iter()) {
                crate::core::patterns::run_pattern_detection(
                    db.conn(),
                    &crate::core::patterns::PatternDetectionArgs {
                        new_drawer_id: drawer_id_p.as_str(),
                        session_id,
                        embedding: vector_p.as_slice(),
                        project_id: project_id.as_deref(),
                        model_id: &model_id,
                        similarity_threshold: config.patterns.similarity_threshold,
                        min_sessions: config.patterns.min_sessions,
                        min_exemplars: config.patterns.min_exemplars,
                        promote_threshold: config.patterns.promote_threshold,
                        top_tags: 5,
                    },
                );
            }
        }

        Ok(Json(IngestResponse {
            drawer_id: response_drawer_id,
            drawer_ids: inserted_drawer_ids,
            chunk_count: chunks.len(),
            dropped: false,
            gating_decision,
            novelty_action,
            near_drawer_id,
            duplicate_warning,
            lock_wait_ms,
            superseded_drawer_id: superseded_response_id,
            fact_check_warnings,
            timings,
            system_warnings: request_system_warnings,
            ..Default::default()
        }))
    }

    #[tool(
        name = "mempal_operation_status",
        description = "Return the current status of an asynchronous ingest operation, including the queue state, stored drawer_id, rejection reason, failure detail, and persisted per-stage timings. Use this to confirm a receipt-based write landed."
    )]
    pub async fn mempal_operation_status(
        &self,
        Parameters(request): Parameters<OperationStatusRequest>,
    ) -> std::result::Result<Json<IngestResponse>, ErrorData> {
        let system_warnings = self
            .system_warnings_with_stale_index_bounded(self.operation_status_deadline)
            .await?;
        let record = match tokio::time::timeout(
            self.operation_status_deadline,
            self.async_queue
                .operation_status(request.operation_id.clone()),
        )
        .await
        {
            Ok(Ok(record)) => record,
            Ok(Err(error)) => {
                return Err(ErrorData::internal_error(
                    format!("queue lookup failed: {error}"),
                    None,
                ));
            }
            Err(_) => {
                return Err(mcp_stage_timeout_error(
                    "mempal_operation_status",
                    "queue lookup",
                    self.operation_status_deadline,
                ));
            }
        }
        .ok_or_else(|| {
            ErrorData::invalid_params(
                format!("operation not found: {}", request.operation_id),
                None,
            )
        })?;

        Ok(Json(operation_record_to_response(record, system_warnings)))
    }

    #[tool(
        name = "mempal_delete",
        description = "Soft-delete a drawer by ID. The drawer is marked with a deleted_at timestamp and excluded from search results, but not physically removed. Use the CLI `mempal purge` to permanently remove soft-deleted drawers. Returns the drawer_id and whether it was found."
    )]
    pub async fn mempal_delete(
        &self,
        Parameters(request): Parameters<DeleteRequest>,
    ) -> std::result::Result<Json<DeleteResponse>, ErrorData> {
        let db = self.open_db()?;
        let deleted = db
            .soft_delete_drawer(&request.drawer_id)
            .map_err(db_error)?;
        let message = if deleted {
            format!("drawer {} soft-deleted", request.drawer_id)
        } else {
            format!("drawer {} not found or already deleted", request.drawer_id)
        };
        Ok(Json(DeleteResponse {
            drawer_id: request.drawer_id,
            deleted,
            message,
            system_warnings: current_system_warnings(),
        }))
    }

    #[tool(
        name = "mempal_rollback",
        description = "Roll back (soft-delete) all drawers created after a given timestamp. Scope can be narrowed by wing/room/project. Use dry_run=true to preview without mutating."
    )]
    pub async fn mempal_rollback(
        &self,
        Parameters(request): Parameters<RollbackRequest>,
    ) -> std::result::Result<Json<RollbackResponse>, ErrorData> {
        let since = normalize_rfc3339_timestamp(&request.since).ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "invalid since timestamp; expected RFC3339: {}",
                    request.since
                ),
                None,
            )
        })?;
        let project_id = match request.project_id.as_deref() {
            Some(project_id) => Some(validate_project_id(project_id).map_err(|error| {
                ErrorData::invalid_params(format!("invalid project scope: {error}"), None)
            })?),
            None => None,
        };
        let db = self.open_db()?;
        let dry_run = request.dry_run.unwrap_or(false);
        let (deleted_count, drawer_ids) = if dry_run {
            let count = db
                .count_drawers_since(
                    &since,
                    request.wing.as_deref(),
                    request.room.as_deref(),
                    project_id.as_deref(),
                )
                .map_err(db_error)?;
            (count.max(0) as usize, Vec::new())
        } else {
            let drawer_ids = db
                .soft_delete_drawers_since(
                    &since,
                    request.wing.as_deref(),
                    request.room.as_deref(),
                    project_id.as_deref(),
                )
                .map_err(db_error)?;
            (drawer_ids.len(), drawer_ids)
        };

        Ok(Json(RollbackResponse {
            since,
            deleted_count,
            drawer_ids,
            dry_run,
            system_warnings: current_system_warnings(),
        }))
    }

    #[tool(
        name = "mempal_lease",
        description = "Coordinate multi-agent access to memory regions via advisory leases. Actions: 'acquire' (lock a resource), 'release' (unlock), 'renew' (extend TTL), 'status' (list active leases). Leases auto-expire after ttl_secs (default 300s) to prevent orphan locks from crashed agents."
    )]
    pub async fn mempal_lease(
        &self,
        Parameters(request): Parameters<LeaseRequest>,
    ) -> std::result::Result<Json<LeaseResponse>, ErrorData> {
        let db = self.open_db()?;
        let ttl = request.ttl_secs.unwrap_or(300);
        match request.action.as_str() {
            "acquire" => {
                let resource = request.resource_path.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("resource_path is required for acquire", None)
                })?;
                let holder = request.holder_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("holder_id is required for acquire", None)
                })?;
                let success = db
                    .lease_acquire(resource, holder, ttl, request.metadata.as_deref())
                    .map_err(db_error)?;
                let lease: Option<LeaseInfoDto> = if success {
                    db.lease_status(Some(resource))
                        .map_err(db_error)?
                        .into_iter()
                        .next()
                        .map(Into::into)
                } else {
                    None
                };
                Ok(Json(LeaseResponse {
                    success,
                    lease,
                    leases: None,
                    error: if success {
                        None
                    } else {
                        Some("resource is held by another agent".to_string())
                    },
                    system_warnings: current_system_warnings(),
                }))
            }
            "release" => {
                let resource = request.resource_path.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("resource_path is required for release", None)
                })?;
                let holder = request.holder_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("holder_id is required for release", None)
                })?;
                let success = db.lease_release(resource, holder).map_err(db_error)?;
                Ok(Json(LeaseResponse {
                    success,
                    lease: None,
                    leases: None,
                    error: if success {
                        None
                    } else {
                        Some("lease not found or wrong holder".to_string())
                    },
                    system_warnings: current_system_warnings(),
                }))
            }
            "renew" => {
                let resource = request.resource_path.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("resource_path is required for renew", None)
                })?;
                let holder = request.holder_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("holder_id is required for renew", None)
                })?;
                let success = db.lease_renew(resource, holder, ttl).map_err(db_error)?;
                let lease: Option<LeaseInfoDto> = if success {
                    db.lease_status(Some(resource))
                        .map_err(db_error)?
                        .into_iter()
                        .next()
                        .map(Into::into)
                } else {
                    None
                };
                Ok(Json(LeaseResponse {
                    success,
                    lease,
                    leases: None,
                    error: if success {
                        None
                    } else {
                        Some("lease not found or wrong holder".to_string())
                    },
                    system_warnings: current_system_warnings(),
                }))
            }
            "status" => {
                let leases: Vec<LeaseInfoDto> = db
                    .lease_status(request.resource_path.as_deref())
                    .map_err(db_error)?
                    .into_iter()
                    .map(Into::into)
                    .collect();
                Ok(Json(LeaseResponse {
                    success: true,
                    lease: None,
                    leases: Some(leases),
                    error: None,
                    system_warnings: current_system_warnings(),
                }))
            }
            other => Err(ErrorData::invalid_params(
                format!(
                    "unknown action '{}'; expected acquire/release/renew/status",
                    other
                ),
                None,
            )),
        }
    }

    #[tool(
        name = "mempal_taxonomy",
        description = "List or edit wing/room taxonomy entries that drive query routing keywords."
    )]
    pub async fn mempal_taxonomy(
        &self,
        Parameters(request): Parameters<TaxonomyRequest>,
    ) -> std::result::Result<Json<TaxonomyResponse>, ErrorData> {
        let db = self.open_db()?;
        match request.action.as_str() {
            "list" => {
                let entries = db
                    .taxonomy_entries()
                    .map_err(db_error)?
                    .into_iter()
                    .map(TaxonomyEntryDto::from)
                    .collect();
                Ok(Json(TaxonomyResponse {
                    action: "list".to_string(),
                    entries,
                    system_warnings: current_system_warnings(),
                }))
            }
            "edit" => {
                let wing = request
                    .wing
                    .ok_or_else(|| ErrorData::invalid_params("missing wing", None))?;
                let room = request
                    .room
                    .ok_or_else(|| ErrorData::invalid_params("missing room", None))?;
                let keywords = request
                    .keywords
                    .ok_or_else(|| ErrorData::invalid_params("missing keywords", None))?;
                let entry = crate::core::types::TaxonomyEntry {
                    wing,
                    room,
                    display_name: None,
                    keywords,
                };
                db.upsert_taxonomy_entry(&entry).map_err(db_error)?;
                Ok(Json(TaxonomyResponse {
                    action: "edit".to_string(),
                    entries: vec![TaxonomyEntryDto::from(entry)],
                    system_warnings: current_system_warnings(),
                }))
            }
            action => Err(ErrorData::invalid_params(
                format!("unsupported taxonomy action: {action}"),
                None,
            )),
        }
    }

    #[tool(
        name = "mempal_field_taxonomy",
        description = "Read-only mind-model field taxonomy guidance. Lists recommended Stage-1 field values such as general, epistemics, software-engineering, debugging, tooling, research, writing, and diary. Guidance only; custom fields remain accepted."
    )]
    async fn mempal_field_taxonomy(
        &self,
    ) -> std::result::Result<Json<FieldTaxonomyResponse>, ErrorData> {
        Ok(Json(FieldTaxonomyResponse {
            entries: field_taxonomy()
                .into_iter()
                .map(FieldTaxonomyEntryDto::from)
                .collect(),
        }))
    }

    #[tool(
        name = "mempal_kg",
        description = "Knowledge graph: add, query, or invalidate triples (subject-predicate-object). Use 'add' to record structured relationships between entities. Use 'query' to find relationships by subject, predicate, or object. Use 'invalidate' to mark a triple as no longer valid."
    )]
    pub async fn mempal_kg(
        &self,
        Parameters(request): Parameters<KgRequest>,
    ) -> std::result::Result<Json<KgResponse>, ErrorData> {
        let block_writes = global_embed_status().should_block_writes();
        let db = self.open_db()?;
        match request.action.as_str() {
            "add" => {
                if block_writes {
                    return Err(degraded_write_error());
                }
                let subject = request
                    .subject
                    .ok_or_else(|| ErrorData::invalid_params("missing subject", None))?;
                let predicate = request
                    .predicate
                    .ok_or_else(|| ErrorData::invalid_params("missing predicate", None))?;
                let object = request
                    .object
                    .ok_or_else(|| ErrorData::invalid_params("missing object", None))?;
                let id = build_triple_id(&subject, &predicate, &object);
                let triple = Triple {
                    id: id.clone(),
                    subject,
                    predicate,
                    object,
                    valid_from: Some(current_timestamp()),
                    valid_to: None,
                    confidence: 1.0,
                    source_drawer: request.source_drawer,
                };
                db.insert_triple(&triple).map_err(db_error)?;
                Ok(Json(KgResponse {
                    action: "add".to_string(),
                    triples: vec![triple_to_dto(&triple)],
                    stats: None,
                    system_warnings: current_system_warnings(),
                }))
            }
            "query" => {
                let active_only = request.active_only.unwrap_or(true);
                let triples = db
                    .query_triples(
                        request.subject.as_deref(),
                        request.predicate.as_deref(),
                        request.object.as_deref(),
                        active_only,
                    )
                    .map_err(db_error)?;
                Ok(Json(KgResponse {
                    action: "query".to_string(),
                    triples: triples.iter().map(triple_to_dto).collect(),
                    stats: None,
                    system_warnings: current_system_warnings(),
                }))
            }
            "invalidate" => {
                if block_writes {
                    return Err(degraded_write_error());
                }
                let triple_id = request
                    .triple_id
                    .ok_or_else(|| ErrorData::invalid_params("missing triple_id", None))?;
                let invalidated = db.invalidate_triple(&triple_id).map_err(db_error)?;
                let message = if invalidated {
                    format!("triple {triple_id} invalidated")
                } else {
                    format!("triple {triple_id} not found or already invalidated")
                };
                Ok(Json(KgResponse {
                    action: message,
                    triples: vec![],
                    stats: None,
                    system_warnings: current_system_warnings(),
                }))
            }
            "timeline" => {
                let entity = request.subject.ok_or_else(|| {
                    ErrorData::invalid_params("missing subject for timeline", None)
                })?;
                let triples = db.timeline_for_entity(&entity).map_err(db_error)?;
                Ok(Json(KgResponse {
                    action: format!("timeline for {entity}"),
                    triples: triples.iter().map(triple_to_dto).collect(),
                    stats: None,
                    system_warnings: current_system_warnings(),
                }))
            }
            "stats" => {
                let stats = db.triple_stats().map_err(db_error)?;
                Ok(Json(KgResponse {
                    action: "stats".to_string(),
                    triples: vec![],
                    stats: Some(KgStatsDto {
                        total: stats.total,
                        active: stats.active,
                        expired: stats.expired,
                        entities: stats.entities,
                        top_predicates: stats.top_predicates,
                    }),
                    system_warnings: current_system_warnings(),
                }))
            }
            action => Err(ErrorData::invalid_params(
                format!("unsupported kg action: {action}"),
                None,
            )),
        }
    }

    #[tool(
        name = "mempal_tunnels",
        description = "Discover or manage cross-wing tunnels. Actions: discover/list passive same-room links, add/list/delete/follow explicit semantic links."
    )]
    async fn mempal_tunnels(
        &self,
        Parameters(request): Parameters<TunnelsRequest>,
    ) -> std::result::Result<Json<TunnelsResponse>, ErrorData> {
        let db = self.open_db()?;
        let action = request.action.as_deref().unwrap_or("discover");
        match action {
            "discover" => Ok(Json(TunnelsResponse {
                tunnels: passive_tunnel_dtos(&db, request.wing.as_deref())?,
                system_warnings: current_system_warnings(),
            })),
            "list" => {
                let kind = request.kind.as_deref().unwrap_or("all");
                let mut tunnels = Vec::new();
                if matches!(kind, "all" | "passive") {
                    tunnels.extend(passive_tunnel_dtos(&db, request.wing.as_deref())?);
                }
                if matches!(kind, "all" | "explicit") {
                    tunnels.extend(
                        db.list_explicit_tunnels(request.wing.as_deref())
                            .map_err(db_error)?
                            .iter()
                            .map(explicit_tunnel_to_dto),
                    );
                }
                if !matches!(kind, "all" | "passive" | "explicit") {
                    return Err(ErrorData::invalid_params(
                        format!("unsupported tunnel kind: {kind}"),
                        None,
                    ));
                }
                Ok(Json(TunnelsResponse {
                    tunnels,
                    system_warnings: current_system_warnings(),
                }))
            }
            "add" => {
                let left = request
                    .left
                    .ok_or_else(|| ErrorData::invalid_params("missing left endpoint", None))?;
                let right = request
                    .right
                    .ok_or_else(|| ErrorData::invalid_params("missing right endpoint", None))?;
                let label = trim_to_option(request.label.as_deref())
                    .ok_or_else(|| ErrorData::invalid_params("missing label", None))?;
                let created_by = self
                    .client_name
                    .lock()
                    .map_err(|_| ErrorData::internal_error("client name lock poisoned", None))?
                    .clone();
                let tunnel = db
                    .create_tunnel(&left.into(), &right.into(), label, created_by.as_deref())
                    .map_err(db_error)?;
                Ok(Json(TunnelsResponse {
                    tunnels: vec![explicit_tunnel_to_dto(&tunnel)],
                    system_warnings: current_system_warnings(),
                }))
            }
            "delete" => {
                let tunnel_id = trim_to_option(request.tunnel_id.as_deref())
                    .ok_or_else(|| ErrorData::invalid_params("missing tunnel_id", None))?;
                if tunnel_id.starts_with("passive_") {
                    return Err(ErrorData::invalid_params(
                        "cannot delete passive tunnel",
                        None,
                    ));
                }
                if !db.delete_explicit_tunnel(tunnel_id).map_err(db_error)? {
                    return Err(ErrorData::invalid_params(
                        format!("tunnel not found: {tunnel_id}"),
                        None,
                    ));
                }
                Ok(Json(TunnelsResponse {
                    tunnels: Vec::new(),
                    system_warnings: current_system_warnings(),
                }))
            }
            "follow" => {
                let from = request
                    .from
                    .ok_or_else(|| ErrorData::invalid_params("missing from endpoint", None))?;
                let max_hops = request.max_hops.unwrap_or(1);
                if !(1..=2).contains(&max_hops) {
                    return Err(ErrorData::invalid_params("max_hops must be 1 or 2", None));
                }
                let tunnels = db
                    .follow_explicit_tunnels(&from.into(), max_hops)
                    .map_err(db_error)?
                    .into_iter()
                    .map(|result| TunnelDto {
                        tunnel_id: result.via_tunnel_id.clone(),
                        kind: "explicit".to_string(),
                        room: None,
                        wings: Vec::new(),
                        left: Some(TunnelEndpointDto::from(&result.endpoint)),
                        right: None,
                        label: None,
                        created_at: None,
                        created_by: None,
                        via_tunnel_id: Some(result.via_tunnel_id),
                        hop: Some(result.hop),
                    })
                    .collect();
                Ok(Json(TunnelsResponse {
                    tunnels,
                    system_warnings: current_system_warnings(),
                }))
            }
            other => Err(ErrorData::invalid_params(
                format!("unsupported tunnels action: {other}"),
                None,
            )),
        }
    }

    #[tool(
        name = "mempal_peek_partner",
        description = "Read the partner coding agent's LIVE session log (Claude Code <-> Codex) without storing it in mempal. Returns the most recent user+assistant messages from their active session file. Use this for CURRENT partner state; use mempal_search for CRYSTALLIZED past decisions. Peek is a pure read -- it never writes to mempal drawers. Pass tool=\"auto\" to infer the partner from MCP ClientInfo, or tool=\"claude\"/\"codex\" explicitly."
    )]
    async fn mempal_peek_partner(
        &self,
        Parameters(request): Parameters<PeekPartnerRequest>,
    ) -> std::result::Result<Json<PeekPartnerResponse>, ErrorData> {
        let tool = Tool::from_str_ci(&request.tool).ok_or_else(|| {
            ErrorData::invalid_params(
                format!(
                    "unknown tool `{}`: expected claude|codex|auto",
                    request.tool
                ),
                None,
            )
        })?;

        let caller_tool = self
            .client_name
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .and_then(|n| Tool::from_str_ci(&n));

        let cwd = std::env::current_dir()
            .map_err(|e| ErrorData::internal_error(format!("cwd unavailable: {e}"), None))?;

        let cowork_req = CoworkPeekRequest {
            tool,
            limit: request.limit.unwrap_or(30),
            since: request.since,
            cwd,
            caller_tool,
            home_override: None,
        };

        let resp = peek_partner(cowork_req).map_err(|e| match e {
            PeekError::CannotInferPartner | PeekError::SelfPeek => {
                ErrorData::invalid_params(e.to_string(), None)
            }
            PeekError::Io(_) | PeekError::Parse(_) => {
                ErrorData::internal_error(e.to_string(), None)
            }
        })?;

        Ok(Json(PeekPartnerResponse {
            partner_tool: resp.partner_tool.as_str().to_string(),
            session_path: resp.session_path,
            session_mtime: resp.session_mtime,
            partner_active: resp.partner_active,
            messages: resp
                .messages
                .into_iter()
                .map(PeekMessageDto::from)
                .collect(),
            truncated: resp.truncated,
            system_warnings: current_system_warnings(),
        }))
    }

    #[tool(
        name = "mempal_cowork_push",
        description = "Proactively deliver a short handoff message to the PARTNER agent's inbox. Partner reads it at their next UserPromptSubmit hook, NOT real-time. Use for transient handoffs too important for mempal_peek_partner and too ephemeral for mempal_ingest. Max 8 KB per message; total inbox capped at 32 KB / 16 messages (InboxFull error means partner must drain). Pass target_tool=\"claude\"/\"codex\" explicitly, or omit to infer partner from MCP client identity. Self-push is rejected."
    )]
    async fn mempal_cowork_push(
        &self,
        Parameters(request): Parameters<CoworkPushRequest>,
    ) -> std::result::Result<Json<CoworkPushResponse>, ErrorData> {
        let caller_name = self.client_name.lock().ok().and_then(|g| g.clone());
        let caller_tool = caller_name
            .as_deref()
            .and_then(Tool::from_str_ci)
            .ok_or_else(|| {
                ErrorData::invalid_params(
                    "cannot infer caller tool from MCP client info (client_name missing or unrecognized)",
                    None,
                )
            })?;

        let target = match request.target_tool.as_deref() {
            Some(name) => Tool::from_target_str(name).ok_or_else(|| {
                ErrorData::invalid_params(
                    format!("unknown target_tool `{name}`: expected claude|codex"),
                    None,
                )
            })?,
            None => caller_tool.partner().ok_or_else(|| {
                ErrorData::invalid_params("caller tool has no partner (tool=auto or unknown)", None)
            })?,
        };

        let mempal_home = crate::cowork::inbox::mempal_home();
        let cwd = PathBuf::from(&request.cwd);
        let pushed_at = current_rfc3339();

        let (path, size) = crate::cowork::inbox::push(
            &mempal_home,
            caller_tool,
            target,
            &cwd,
            request.content,
            pushed_at.clone(),
        )
        .map_err(|e| match e {
            crate::cowork::inbox::InboxError::SelfPush(_)
            | crate::cowork::inbox::InboxError::MessageTooLarge(_)
            | crate::cowork::inbox::InboxError::InvalidCwd(_)
            | crate::cowork::inbox::InboxError::InboxFull { .. } => {
                ErrorData::invalid_params(e.to_string(), None)
            }
            _ => ErrorData::internal_error(e.to_string(), None),
        })?;

        Ok(Json(CoworkPushResponse {
            target_tool: target.dir_name().to_string(),
            inbox_path: path.to_string_lossy().to_string(),
            pushed_at,
            inbox_size_after: size,
            system_warnings: current_system_warnings(),
        }))
    }

    #[tool(
        name = "mempal_doctor",
        description = "MCP runtime diagnostics for mempal install/schema compatibility and server-advertised runtime tools. Read-only; does not migrate or create the database."
    )]
    async fn mempal_doctor(
        &self,
        Parameters(_request): Parameters<DoctorRequest>,
    ) -> std::result::Result<Json<DoctorResponse>, ErrorData> {
        let advertised_tools = self.tool_router.list_all();
        let mcp = DoctorMcpDto {
            required_tools: REQUIRED_MCP_TOOLS
                .iter()
                .map(|name| DoctorToolDto {
                    name: (*name).to_string(),
                    advertised: advertised_tools.iter().any(|tool| tool.name == *name),
                })
                .collect(),
            phase3_actions: PHASE3_ACTIONS
                .iter()
                .map(|action| (*action).to_string())
                .collect(),
            cowork_bus_actions: COWORK_BUS_ACTIONS
                .iter()
                .map(|action| (*action).to_string())
                .collect(),
        };
        Ok(Json(DoctorResponse::from_report(
            build_doctor_report(&self.db_path),
            mcp,
        )))
    }

    #[tool(
        name = "mempal_brief",
        description = "Assemble a deterministic citation-first cognitive brief from memory. Returns summary, key facts, evidence, cards, unresolved items, uncertainty, and next actions without LLM synthesis or writes."
    )]
    async fn mempal_brief(
        &self,
        Parameters(request): Parameters<BriefMcpRequest>,
    ) -> std::result::Result<Json<BriefMcpResponse>, ErrorData> {
        let max_items = request.max_items.unwrap_or(12);
        if max_items == 0 {
            return Err(ErrorData::invalid_params(
                "max_items must be greater than 0",
                None,
            ));
        }
        let domain = parse_domain(request.domain.as_deref())?.unwrap_or(MemoryDomain::Project);
        let cwd = match request.cwd.as_deref() {
            Some(value) if !value.trim().is_empty() => PathBuf::from(value),
            Some(_) => {
                return Err(ErrorData::invalid_params(
                    "cwd must not be empty when provided",
                    None,
                ));
            }
            None => std::env::current_dir().map_err(|error| {
                ErrorData::internal_error(
                    format!("failed to read current directory: {error}"),
                    None,
                )
            })?,
        };

        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let query_vector = embedder
            .embed(&[request.query.as_str()])
            .await
            .map_err(|error| ErrorData::internal_error(format!("embedding failed: {error}"), None))?
            .into_iter()
            .next()
            .ok_or_else(|| ErrorData::internal_error("embedder returned no query vector", None))?;
        let db = self.open_db()?;
        let context = assemble_context_with_vector(
            &db,
            crate::context::ContextRequest {
                query: request.query,
                domain,
                field: request
                    .field
                    .unwrap_or_else(|| anchor::DEFAULT_FIELD.to_string()),
                cwd,
                include_evidence: true,
                include_cards: true,
                max_items,
                dao_tian_limit: request.dao_tian_limit.unwrap_or(1),
                project_id: None,
                trigger: None,
                context_cfg_override: None,
                include_distill_suggestions: false,
            },
            &query_vector,
        )
        .map_err(|error| ErrorData::internal_error(format!("brief failed: {error}"), None))?;
        let brief = brief_from_context(context);
        Ok(Json(BriefMcpResponse::from(brief)))
    }

    #[tool(
        name = "mempal_cowork_bus",
        description = "Multi-agent cowork bus for concrete agent instances in one project. \
                       Actions: register/list/send/broadcast/drain/events/deliveries/ack/heartbeat/channel_set/channel_list/channel_send/tmux_peek/doctor/session_create/session_list/session_status/session_close/handoff/capture. Uses explicit agent_id \
                       values such as claude-main, codex-a, codex-b, per-agent inbox files, \
                       and append-only events under ~/.mempal/cowork-bus/<project>. This is separate from legacy \
                       mempal_cowork_push partner routing and does not infer concrete instances \
                       from MCP client names. Most actions are file-backed runtime ops; action=capture writes \
                       evidence only when execute=true."
    )]
    async fn mempal_cowork_bus(
        &self,
        Parameters(request): Parameters<CoworkBusRequest>,
    ) -> std::result::Result<Json<CoworkBusResponse>, ErrorData> {
        use crate::cowork::bus::{self, RegisterAgentRequest, SendRequest};

        let mempal_home = crate::cowork::inbox::mempal_home();
        let cwd = PathBuf::from(&request.cwd);
        let action = request.action.as_str();

        match action {
            "register" => {
                let agent_id = required_bus_field(request.agent_id, "agent_id", action)?;
                let tool = required_bus_field(request.tool, "tool", action)?;
                let record = bus::register_agent(
                    &mempal_home,
                    &cwd,
                    RegisterAgentRequest {
                        agent_id,
                        tool,
                        transport: request.transport.unwrap_or_else(|| "inbox".to_string()),
                        tmux_target: request.tmux_target,
                    },
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: vec![agent_record_to_dto(record)],
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "list" => {
                let statuses =
                    bus::list_agent_status_at(&mempal_home, &cwd, request.now.as_deref())
                        .map_err(bus_error_to_mcp)?
                        .into_iter()
                        .map(agent_status_to_dto)
                        .collect();
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: statuses,
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "send" | "broadcast" => {
                let from = required_bus_field(request.from, "from", action)?;
                if request.to.is_empty() {
                    return Err(ErrorData::invalid_params(
                        format!("action `{action}` requires at least one `to` agent_id"),
                        None,
                    ));
                }
                if action == "send" && request.to.len() != 1 {
                    return Err(ErrorData::invalid_params(
                        "action `send` requires exactly one `to`; use broadcast for fanout",
                        None,
                    ));
                }
                let message = required_bus_field(request.message, "message", action)?;
                let report = bus::send(
                    &mempal_home,
                    &cwd,
                    SendRequest {
                        from,
                        targets: request.to,
                        message,
                        operation: if action == "send" {
                            bus::SendOperation::Send
                        } else {
                            bus::SendOperation::Broadcast
                        },
                        thread_id: request.thread_id,
                        channel: request.channel,
                    },
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: report.delivered.into_iter().map(delivery_to_dto).collect(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "drain" => {
                let agent_id = required_bus_field(request.agent_id, "agent_id", action)?;
                let messages = bus::drain_agent(&mempal_home, &cwd, &agent_id)
                    .map_err(bus_error_to_mcp)?
                    .into_iter()
                    .map(message_to_dto)
                    .collect();
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages,
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "events" => {
                let events = bus::list_events(&mempal_home, &cwd, request.limit)
                    .map_err(bus_error_to_mcp)?
                    .into_iter()
                    .map(event_to_dto)
                    .collect();
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events,
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "deliveries" => {
                let deliveries =
                    bus::list_delivery_statuses(&mempal_home, &cwd, request.agent_id.as_deref())
                        .map_err(bus_error_to_mcp)?
                        .into_iter()
                        .map(delivery_status_to_dto)
                        .collect();
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries,
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "ack" => {
                let agent_id = required_bus_field(request.agent_id, "agent_id", action)?;
                let message_id = required_bus_field(request.message_id, "message_id", action)?;
                let status = bus::ack_delivery(&mempal_home, &cwd, &agent_id, &message_id)
                    .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: vec![delivery_status_to_dto(status)],
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "heartbeat" => {
                let agent_id = required_bus_field(request.agent_id, "agent_id", action)?;
                let record =
                    bus::heartbeat_agent(&mempal_home, &cwd, &agent_id, request.seen_at.as_deref())
                        .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: vec![agent_record_to_dto(record)],
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "channel_set" => {
                let channel = required_bus_field(request.channel, "channel", action)?;
                if request.agents.is_empty() {
                    return Err(ErrorData::invalid_params(
                        "action `channel_set` requires at least one `agents` entry",
                        None,
                    ));
                }
                let channel = bus::set_channel(&mempal_home, &cwd, &channel, request.agents)
                    .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: vec![channel_to_dto(channel)],
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "channel_list" => {
                let channels = bus::list_channels(&mempal_home, &cwd)
                    .map_err(bus_error_to_mcp)?
                    .into_iter()
                    .map(channel_to_dto)
                    .collect();
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels,
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "channel_send" => {
                let from = required_bus_field(request.from, "from", action)?;
                let channel = required_bus_field(request.channel, "channel", action)?;
                let message = required_bus_field(request.message, "message", action)?;
                let report = bus::send_channel(
                    &mempal_home,
                    &cwd,
                    from,
                    channel,
                    message,
                    request.thread_id,
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: report.delivered.into_iter().map(delivery_to_dto).collect(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "tmux_peek" => {
                let agent_id = required_bus_field(request.agent_id, "agent_id", action)?;
                let peek = bus::tmux_peek_agent(
                    &mempal_home,
                    &cwd,
                    &agent_id,
                    request.lines.unwrap_or(80),
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: Some(tmux_peek_to_dto(peek)),
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "doctor" => {
                let report = bus::doctor(
                    &mempal_home,
                    &cwd,
                    request.now.as_deref(),
                    request.probe_tmux.unwrap_or(false),
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: Some(doctor_to_dto(report)),
                    sessions: Vec::new(),
                    handoff: None,
                    capture: None,
                }))
            }
            "session_create" => {
                let session_id = required_bus_field(request.session_id, "session_id", action)?;
                let title = required_bus_field(request.title, "title", action)?;
                if request.agents.is_empty() {
                    return Err(ErrorData::invalid_params(
                        "action `session_create` requires at least one `agents` entry",
                        None,
                    ));
                }
                let session = bus::create_session(
                    &mempal_home,
                    &cwd,
                    bus::CreateSessionRequest {
                        session_id,
                        title,
                        goal: request.goal,
                        agents: request.agents,
                        channels: if let Some(channel) = request.channel {
                            vec![channel]
                        } else {
                            Vec::new()
                        },
                        thread_id: request.thread_id,
                    },
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: vec![session_to_dto(session)],
                    handoff: None,
                    capture: None,
                }))
            }
            "session_list" => {
                let sessions = bus::list_sessions(&mempal_home, &cwd)
                    .map_err(bus_error_to_mcp)?
                    .into_iter()
                    .map(session_to_dto)
                    .collect();
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions,
                    handoff: None,
                    capture: None,
                }))
            }
            "session_status" => {
                let session_id = required_bus_field(request.session_id, "session_id", action)?;
                let status = required_bus_field(request.status, "status", action)?;
                let session = bus::update_session_status(&mempal_home, &cwd, &session_id, &status)
                    .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: vec![session_to_dto(session)],
                    handoff: None,
                    capture: None,
                }))
            }
            "session_close" => {
                let session_id = required_bus_field(request.session_id, "session_id", action)?;
                let session = bus::update_session_status(&mempal_home, &cwd, &session_id, "closed")
                    .map_err(bus_error_to_mcp)?;
                let capture = if request.capture.unwrap_or(false) {
                    let execute = request.execute.unwrap_or(false);
                    let db = if execute { Some(self.open_db()?) } else { None };
                    Some(
                        bus::capture_handoff_to_memory(
                            db.as_ref(),
                            &mempal_home,
                            &cwd,
                            bus::CoworkCaptureRequest {
                                summary_source: request
                                    .summary_source
                                    .unwrap_or_else(|| "handoff".to_string()),
                                wing: request.wing.unwrap_or_else(|| "cowork-capture".to_string()),
                                room: request.room,
                                thread_id: request.thread_id,
                                channel: request.channel,
                                session_id: Some(session_id),
                                note: request.note,
                                execute,
                            },
                        )
                        .map_err(bus_error_to_mcp)?,
                    )
                } else {
                    None
                };
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: vec![session_to_dto(session)],
                    handoff: None,
                    capture: capture.map(capture_to_dto),
                }))
            }
            "handoff" => {
                let summary = bus::build_handoff_summary(
                    &mempal_home,
                    &cwd,
                    bus::HandoffFilters {
                        thread_id: request.thread_id,
                        channel: request.channel,
                        session_id: request.session_id,
                        limit: request.limit,
                    },
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: Some(handoff_to_dto(summary)),
                    capture: None,
                }))
            }
            "capture" => {
                let execute = request.execute.unwrap_or(false);
                let db = if execute { Some(self.open_db()?) } else { None };
                let report = bus::capture_handoff_to_memory(
                    db.as_ref(),
                    &mempal_home,
                    &cwd,
                    bus::CoworkCaptureRequest {
                        summary_source: request
                            .summary_source
                            .unwrap_or_else(|| "handoff".to_string()),
                        wing: request.wing.unwrap_or_else(|| "cowork-capture".to_string()),
                        room: request.room,
                        thread_id: request.thread_id,
                        channel: request.channel,
                        session_id: request.session_id,
                        note: request.note,
                        execute,
                    },
                )
                .map_err(bus_error_to_mcp)?;
                Ok(Json(CoworkBusResponse {
                    action: action.to_string(),
                    agents: Vec::new(),
                    delivered: Vec::new(),
                    messages: Vec::new(),
                    events: Vec::new(),
                    deliveries: Vec::new(),
                    channels: Vec::new(),
                    tmux_peek: None,
                    doctor: None,
                    sessions: Vec::new(),
                    handoff: None,
                    capture: Some(capture_to_dto(report)),
                }))
            }
            other => Err(ErrorData::invalid_params(
                format!(
                    "unknown action `{other}`: expected register|list|send|broadcast|drain|events|deliveries|ack|heartbeat|channel_set|channel_list|channel_send|tmux_peek|doctor|session_create|session_list|session_status|session_close|handoff|capture"
                ),
                None,
            )),
        }
    }

    #[tool(
        name = "mempal_fact_check",
        description = "Detect contradictions in text against KG triples + known entities. Returns SimilarNameConflict (similar-name typos), RelationContradiction (incompatible predicate for same endpoints), and StaleFact (KG valid_to expired) issues. Pure read, zero LLM, zero network, deterministic. Call before ingesting decisions that assert relationships between named entities to catch typos or outdated assumptions early."
    )]
    async fn mempal_fact_check(
        &self,
        Parameters(request): Parameters<FactCheckRequest>,
    ) -> std::result::Result<Json<FactCheckResponse>, ErrorData> {
        let db = self.open_db()?;
        let now_secs =
            crate::factcheck::resolve_now(request.now.as_deref()).map_err(fact_check_error)?;
        let scope =
            crate::factcheck::validate_scope(request.wing.as_deref(), request.room.as_deref())
                .map_err(fact_check_error)?;

        let report = tokio::task::block_in_place(|| {
            crate::factcheck::check(&request.text, &db, now_secs, scope)
        })
        .map_err(fact_check_error)?;

        // Apply stale penalty to drawers associated with StaleFact triples (P13).
        let stale_penalty = ConfigHandle::current().importance.stale_penalty;
        for issue in &report.issues {
            if let crate::factcheck::FactIssue::StaleFact {
                source_drawer: Some(drawer_id),
                ..
            } = issue
            {
                if let Err(err) = db.apply_stale_penalty_to_drawer(drawer_id, stale_penalty) {
                    tracing::warn!(drawer_id, error = %err, "stale penalty application failed");
                }
            }
        }

        Ok(Json(FactCheckResponse {
            issues: report.issues,
            checked_entities: report.checked_entities,
            kg_triples_scanned: report.kg_triples_scanned,
            repair_packages: report.repair_packages,
            system_warnings: current_system_warnings(),
        }))
    }

    #[tool(
        name = "mempal_skill",
        description = "Skill crystallization (P15): manage skills promoted from validated recurring patterns. Actions: list (list skills, optional status/project_id filter), show (full detail for one skill), promote (promote an active pattern to a probationary skill — you MUST provide name and trigger_description; mempal does NOT generate these), adopt (signal the skill was useful — adoption_count += 1, may promote to active), reject (signal the skill was not useful — rejection_count += 1, may auto-retire), retire (manually retire a skill). Only active skills are injected into context. Probationary skills need adopt signals to graduate. eta = adoption / (adoption + rejection + 1.0), computed at query time."
    )]
    pub async fn mempal_skill(
        &self,
        Parameters(request): Parameters<SkillRequest>,
    ) -> std::result::Result<Json<SkillResponse>, ErrorData> {
        let db = self.open_db()?;
        let config = ConfigHandle::current();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), &config)
            .await?;

        if !crate::core::skills::skills_table_exists(db.conn()) {
            return Err(ErrorData::internal_error(
                "skills table not yet created — run `mempal init` to apply migrations",
                None,
            ));
        }

        match request.action.as_str() {
            "list" => {
                let skills = tokio::task::block_in_place(|| {
                    crate::core::skills::list_skills(
                        db.conn(),
                        request.status.as_deref(),
                        project_id.as_deref(),
                    )
                })
                .map_err(|e| ErrorData::internal_error(format!("list_skills failed: {e}"), None))?;

                let dtos: Vec<SkillSummaryDto> = skills
                    .iter()
                    .map(|s| SkillSummaryDto {
                        skill_id: s.skill_id.clone(),
                        name: s.name.clone(),
                        trigger_description: s.trigger_description.clone(),
                        eta: s.eta(),
                        status: s.status.as_str().to_string(),
                        adoption_count: s.adoption_count,
                        rejection_count: s.rejection_count,
                    })
                    .collect();

                Ok(Json(SkillResponse {
                    action: "list".to_string(),
                    status: None,
                    skill: None,
                    skills: dtos,
                    message: None,
                }))
            }

            "show" => {
                let skill_id = request.skill_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("skill_id is required for show", None)
                })?;
                let skill = tokio::task::block_in_place(|| {
                    crate::core::skills::get_skill(db.conn(), skill_id)
                })
                .map_err(|e| ErrorData::internal_error(format!("get_skill failed: {e}"), None))?
                .ok_or_else(|| {
                    ErrorData::invalid_params(format!("skill not found: {skill_id}"), None)
                })?;

                Ok(Json(SkillResponse {
                    action: "show".to_string(),
                    status: Some(skill.status.as_str().to_string()),
                    skill: Some(skill_to_dto(&skill)),
                    skills: vec![],
                    message: None,
                }))
            }

            "promote" => {
                let pattern_id = request.pattern_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("pattern_id is required for promote", None)
                })?;
                let name = request.name.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params(
                        "name is required for promote (agent must provide, mempal does not generate)",
                        None,
                    )
                })?;
                let trigger_description = request.trigger_description.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params(
                        "trigger_description is required for promote (agent must provide, mempal does not generate)",
                        None,
                    )
                })?;

                let skill_min_sessions = config.skills.skill_min_sessions;
                let skill = tokio::task::block_in_place(|| {
                    crate::core::skills::promote_pattern_to_skill(
                        db.conn(),
                        &crate::core::skills::PromoteArgs {
                            pattern_id,
                            name,
                            trigger_description,
                            skill_min_sessions,
                            project_id: project_id.as_deref(),
                        },
                    )
                })
                .map_err(|e| match e {
                    crate::core::skills::PromotionError::PatternNotFound(_)
                    | crate::core::skills::PromotionError::PatternNotActive(_)
                    | crate::core::skills::PromotionError::InsufficientSessions(_, _)
                    | crate::core::skills::PromotionError::SkillAlreadyExists => {
                        ErrorData::invalid_params(e.to_string(), None)
                    }
                    crate::core::skills::PromotionError::Db(db_err) => {
                        ErrorData::internal_error(format!("promote_skill db error: {db_err}"), None)
                    }
                })?;

                Ok(Json(SkillResponse {
                    action: "promote".to_string(),
                    status: Some(skill.status.as_str().to_string()),
                    skill: Some(skill_to_dto(&skill)),
                    skills: vec![],
                    message: Some(format!("skill '{}' created as probationary", skill.name)),
                }))
            }

            "adopt" => {
                let skill_id = request.skill_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("skill_id is required for adopt", None)
                })?;
                let active_threshold = config.skills.active_threshold;
                let new_status = tokio::task::block_in_place(|| {
                    crate::core::skills::adopt_skill(db.conn(), skill_id, active_threshold)
                })
                .map_err(|e| ErrorData::internal_error(format!("adopt_skill failed: {e}"), None))?
                .ok_or_else(|| {
                    ErrorData::invalid_params(format!("skill not found: {skill_id}"), None)
                })?;

                Ok(Json(SkillResponse {
                    action: "adopt".to_string(),
                    status: Some(new_status.as_str().to_string()),
                    skill: None,
                    skills: vec![],
                    message: Some(format!(
                        "adoption recorded; skill status: {}",
                        new_status.as_str()
                    )),
                }))
            }

            "reject" => {
                let skill_id = request.skill_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("skill_id is required for reject", None)
                })?;
                let retire_threshold = config.skills.retire_threshold;
                let new_status = tokio::task::block_in_place(|| {
                    crate::core::skills::reject_skill(db.conn(), skill_id, retire_threshold)
                })
                .map_err(|e| ErrorData::internal_error(format!("reject_skill failed: {e}"), None))?
                .ok_or_else(|| {
                    ErrorData::invalid_params(format!("skill not found: {skill_id}"), None)
                })?;

                Ok(Json(SkillResponse {
                    action: "reject".to_string(),
                    status: Some(new_status.as_str().to_string()),
                    skill: None,
                    skills: vec![],
                    message: Some(format!(
                        "rejection recorded; skill status: {}",
                        new_status.as_str()
                    )),
                }))
            }

            "retire" => {
                let skill_id = request.skill_id.as_deref().ok_or_else(|| {
                    ErrorData::invalid_params("skill_id is required for retire", None)
                })?;
                let found = tokio::task::block_in_place(|| {
                    crate::core::skills::retire_skill(db.conn(), skill_id)
                })
                .map_err(|e| {
                    ErrorData::internal_error(format!("retire_skill failed: {e}"), None)
                })?;

                if !found {
                    return Err(ErrorData::invalid_params(
                        format!("skill not found or already retired: {skill_id}"),
                        None,
                    ));
                }

                Ok(Json(SkillResponse {
                    action: "retire".to_string(),
                    status: Some("retired".to_string()),
                    skill: None,
                    skills: vec![],
                    message: Some(format!("skill {skill_id} retired")),
                }))
            }

            unknown => Err(ErrorData::invalid_params(
                format!(
                    "unknown action '{unknown}'; valid: list, show, promote, adopt, reject, retire"
                ),
                None,
            )),
        }
    }

    #[tool(
        name = "mempal_phase3",
        description = "Phase-3 runtime adoption evidence and readiness gates. Actions: guidance/instrumentation_policy/prepare_record/capture/evaluator_advise/default_proposal/rollback_control/check_record/record_checked/review/readiness/record/list/stats/gate/research_validate_plan/research_ingest_plan. Guidance explains when agents should record used/accepted/rejected/miss/rollback signals; instrumentation_policy defines opt-in live instrumentation boundaries without writing; prepare_record validates and returns record inputs without writing; capture maps surface/outcome observations into checked record inputs and writes only with execute=true; evaluator_advise returns deterministic advisory-only evaluator output and a surface=evaluator capture plan without lifecycle authority; default_proposal combines readiness with rollback criteria without changing defaults; rollback_control evaluates card-context rollback evidence without writing; check_record evaluates record quality without writing; record_checked runs the quality gate before writing; review summarizes adoption evidence without writing; readiness evaluates default eligibility without writing; record appends runtime_adoption_events; list/stats/gate are read-only; research_validate_plan validates external research report JSON; research_ingest_plan previews evidence drawer refs and distill suggestions without ingesting or promoting knowledge."
    )]
    async fn mempal_phase3(
        &self,
        Parameters(request): Parameters<Phase3Request>,
    ) -> std::result::Result<Json<Phase3Response>, ErrorData> {
        let action = trim_to_option(Some(request.action.as_str()))
            .ok_or_else(|| ErrorData::invalid_params("action must not be empty", None))?;

        match action {
            "guidance" => Ok(Json(Phase3Response {
                guidance: Some(runtime_adoption_guidance().into()),
                instrumentation_policy: None,
                record_plan: None,
                record_quality: None,
                record_checked: None,
                review_report: None,
                readiness_report: None,
                event: None,
                events: Vec::new(),
                stats: None,
                analytics: None,
                gate: None,
                research_plan: None,
                research_ingest_plan: None,
                evaluator_advice: None,
                default_proposal: None,
                rollback_control: None,
            })),
            "instrumentation_policy" => Ok(Json(Phase3Response {
                guidance: None,
                instrumentation_policy: Some(runtime_adoption_instrumentation_policy().into()),
                record_plan: None,
                record_quality: None,
                record_checked: None,
                review_report: None,
                readiness_report: None,
                event: None,
                events: Vec::new(),
                stats: None,
                analytics: None,
                gate: None,
                research_plan: None,
                research_ingest_plan: None,
                evaluator_advice: None,
                default_proposal: None,
                rollback_control: None,
            })),
            "prepare_record" => {
                let track = parse_runtime_adoption_track(required_string(
                    request.track.as_deref(),
                    "track",
                )?)?;
                let signal = parse_runtime_adoption_signal(required_string(
                    request.signal.as_deref(),
                    "signal",
                )?)?;
                let feature = required_string(request.feature.as_deref(), "feature")?.to_string();
                let plan = prepare_runtime_adoption_record(RuntimeAdoptionRecordPlanInput {
                    id: trim_to_owned(request.id.as_deref()),
                    track: runtime_adoption_track_slug(&track).to_string(),
                    signal: runtime_adoption_signal_slug(&signal).to_string(),
                    feature,
                    query: trim_to_owned(request.query.as_deref()),
                    context_hash: trim_to_owned(request.context_hash.as_deref()),
                    card_id: trim_to_owned(request.card_id.as_deref()),
                    evaluator_id: trim_to_owned(request.evaluator_id.as_deref()),
                    research_report_id: trim_to_owned(request.research_report_id.as_deref()),
                    note: trim_to_owned(request.note.as_deref()),
                    metadata: request.metadata,
                });
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: Some(plan.into()),
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "capture" => {
                let surface = required_string(request.surface.as_deref(), "surface")?.to_string();
                let outcome = required_string(request.outcome.as_deref(), "outcome")?.to_string();
                let record_input =
                    capture_runtime_adoption_record_input(RuntimeAdoptionCaptureInput {
                        id: trim_to_owned(request.id.as_deref()),
                        surface: surface.clone(),
                        outcome: outcome.clone(),
                        query: trim_to_owned(request.query.as_deref()),
                        context_hash: trim_to_owned(request.context_hash.as_deref()),
                        card_id: trim_to_owned(request.card_id.as_deref()),
                        evaluator_id: trim_to_owned(request.evaluator_id.as_deref()),
                        research_report_id: trim_to_owned(request.research_report_id.as_deref()),
                        note: trim_to_owned(request.note.as_deref()),
                        metadata: request.metadata,
                    })
                    .map_err(|error| ErrorData::invalid_params(error, None))?;
                let mut capture = prepare_runtime_adoption_capture(
                    surface,
                    outcome,
                    request.execute.unwrap_or(false),
                    record_input.clone(),
                );
                if request.execute.unwrap_or(false) {
                    let db = self.open_db()?;
                    let track = parse_runtime_adoption_track(&record_input.track)?;
                    let signal = parse_runtime_adoption_signal(&record_input.signal)?;
                    let should_write = should_write_checked_record(
                        &capture.record_quality,
                        request.allow_warnings.unwrap_or(false),
                    );
                    let event = if should_write {
                        let event = RuntimeAdoptionEvent {
                            id: record_input.id.unwrap_or_else(|| {
                                phase3_event_id(&track, &signal, &record_input.feature)
                            }),
                            track,
                            signal,
                            feature: record_input.feature,
                            query: record_input.query,
                            context_hash: record_input.context_hash,
                            card_id: record_input.card_id,
                            evaluator_id: record_input.evaluator_id,
                            research_report_id: record_input.research_report_id,
                            note: record_input.note,
                            metadata: record_input.metadata,
                            created_at: current_timestamp(),
                        };
                        db.insert_runtime_adoption_event(&event).map_err(|error| {
                            ErrorData::internal_error(
                                format!("failed to insert runtime adoption event: {error}"),
                                None,
                            )
                        })?;
                        Some(event)
                    } else {
                        None
                    };
                    capture.writes = event.is_some();
                    capture.record_checked = Some(RuntimeAdoptionCheckedRecordReport {
                        writes: event.is_some(),
                        blocked: event.is_none(),
                        record_quality: capture.record_quality.clone(),
                        event,
                    });
                }
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: Some(capture.record_plan.into()),
                    record_quality: Some(capture.record_quality.into()),
                    record_checked: capture.record_checked.map(Into::into),
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "evaluator_advise" => {
                let report = evaluator_advice(EvaluatorAdviceInput {
                    evaluator_id: required_string(request.evaluator_id.as_deref(), "evaluator_id")?
                        .to_string(),
                    subject_kind: required_string(request.subject_kind.as_deref(), "subject_kind")?
                        .to_string(),
                    subject_id: required_string(request.subject_id.as_deref(), "subject_id")?
                        .to_string(),
                    proposed_action: required_string(
                        request.proposed_action.as_deref(),
                        "proposed_action",
                    )?
                    .to_string(),
                    evidence_refs: request.evidence_refs.unwrap_or_default(),
                    counterexample_refs: request.counterexample_refs.unwrap_or_default(),
                    risk_notes: request.risk_notes.unwrap_or_default(),
                    note: trim_to_owned(request.note.as_deref()),
                })
                .map_err(|error| ErrorData::invalid_params(error, None))?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: Some(report.into()),
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "default_proposal" => {
                let candidate = required_string(request.candidate.as_deref(), "candidate")?;
                let report = match candidate {
                    "card-context" => {
                        let db = self.open_db()?;
                        let events = db
                            .list_runtime_adoption_events(
                                &RuntimeAdoptionFilter {
                                    track: Some(RuntimeAdoptionTrack::CardContext),
                                    feature: Some("include_cards".to_string()),
                                },
                                10_000,
                            )
                            .map_err(|error| {
                                ErrorData::internal_error(
                                    format!("failed to list runtime adoption events: {error}"),
                                    None,
                                )
                            })?;
                        card_context_default_proposal(
                            &events,
                            request.rollback_criteria.unwrap_or_default(),
                        )
                    }
                    other => {
                        return Err(ErrorData::invalid_params(
                            format!("unsupported phase3 default proposal candidate: {other}"),
                            None,
                        ));
                    }
                };
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: Some(report.into()),
                    rollback_control: None,
                }))
            }
            "rollback_control" => {
                let candidate = required_string(request.candidate.as_deref(), "candidate")?;
                let report = match candidate {
                    "card-context" => {
                        if request.execute.unwrap_or(false) {
                            return Err(ErrorData::invalid_params(
                                "rollback_control execute is only supported by CLI in P79",
                                None,
                            ));
                        }
                        let db = self.open_db()?;
                        let events = db
                            .list_runtime_adoption_events(
                                &RuntimeAdoptionFilter {
                                    track: Some(RuntimeAdoptionTrack::CardContext),
                                    feature: Some("include_cards".to_string()),
                                },
                                10_000,
                            )
                            .map_err(|error| {
                                ErrorData::internal_error(
                                    format!("failed to list runtime adoption events: {error}"),
                                    None,
                                )
                            })?;
                        card_context_rollback_control(
                            &events,
                            ConfigHandle::current().context.include_cards_default,
                            false,
                        )
                    }
                    other => {
                        return Err(ErrorData::invalid_params(
                            format!("unsupported phase3 rollback-control candidate: {other}"),
                            None,
                        ));
                    }
                };
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: Some(report.into()),
                }))
            }
            "check_record" => {
                let track = parse_runtime_adoption_track(required_string(
                    request.track.as_deref(),
                    "track",
                )?)?;
                let signal = parse_runtime_adoption_signal(required_string(
                    request.signal.as_deref(),
                    "signal",
                )?)?;
                let feature = request.feature.unwrap_or_default();
                let input = RuntimeAdoptionRecordPlanInput {
                    id: trim_to_owned(request.id.as_deref()),
                    track: runtime_adoption_track_slug(&track).to_string(),
                    signal: runtime_adoption_signal_slug(&signal).to_string(),
                    feature,
                    query: trim_to_owned(request.query.as_deref()),
                    context_hash: trim_to_owned(request.context_hash.as_deref()),
                    card_id: trim_to_owned(request.card_id.as_deref()),
                    evaluator_id: trim_to_owned(request.evaluator_id.as_deref()),
                    research_report_id: trim_to_owned(request.research_report_id.as_deref()),
                    note: trim_to_owned(request.note.as_deref()),
                    metadata: request.metadata,
                };
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: Some(check_runtime_adoption_record(&input).into()),
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "record_checked" => {
                let db = self.open_db()?;
                let track = parse_runtime_adoption_track(required_string(
                    request.track.as_deref(),
                    "track",
                )?)?;
                let signal = parse_runtime_adoption_signal(required_string(
                    request.signal.as_deref(),
                    "signal",
                )?)?;
                let feature = request.feature.unwrap_or_default();
                let input = RuntimeAdoptionRecordPlanInput {
                    id: trim_to_owned(request.id.as_deref()),
                    track: runtime_adoption_track_slug(&track).to_string(),
                    signal: runtime_adoption_signal_slug(&signal).to_string(),
                    feature,
                    query: trim_to_owned(request.query.as_deref()),
                    context_hash: trim_to_owned(request.context_hash.as_deref()),
                    card_id: trim_to_owned(request.card_id.as_deref()),
                    evaluator_id: trim_to_owned(request.evaluator_id.as_deref()),
                    research_report_id: trim_to_owned(request.research_report_id.as_deref()),
                    note: trim_to_owned(request.note.as_deref()),
                    metadata: request.metadata,
                };
                let quality = check_runtime_adoption_record(&input);
                let should_write =
                    should_write_checked_record(&quality, request.allow_warnings.unwrap_or(false));
                let event = if should_write {
                    let event = RuntimeAdoptionEvent {
                        id: input
                            .id
                            .unwrap_or_else(|| phase3_event_id(&track, &signal, &input.feature)),
                        track,
                        signal,
                        feature: input.feature,
                        query: input.query,
                        context_hash: input.context_hash,
                        card_id: input.card_id,
                        evaluator_id: input.evaluator_id,
                        research_report_id: input.research_report_id,
                        note: input.note,
                        metadata: input.metadata,
                        created_at: current_timestamp(),
                    };
                    db.insert_runtime_adoption_event(&event).map_err(|error| {
                        ErrorData::internal_error(
                            format!("failed to insert runtime adoption event: {error}"),
                            None,
                        )
                    })?;
                    Some(event)
                } else {
                    None
                };
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: Some(
                        RuntimeAdoptionCheckedRecordReport {
                            writes: event.is_some(),
                            blocked: event.is_none(),
                            record_quality: quality,
                            event,
                        }
                        .into(),
                    ),
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "review" => {
                let db = self.open_db()?;
                let track = parse_runtime_adoption_track_opt(request.track.as_deref())?;
                let signal = request
                    .signal
                    .as_deref()
                    .map(parse_runtime_adoption_signal)
                    .transpose()?;
                let feature = trim_to_owned(request.feature.as_deref());
                let limit = request.limit.unwrap_or(10_000);
                let events = db
                    .list_runtime_adoption_events(
                        &RuntimeAdoptionFilter {
                            track: track.clone(),
                            feature: feature.clone(),
                        },
                        limit,
                    )
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("failed to list runtime adoption events: {error}"),
                            None,
                        )
                    })?;
                let report = review_runtime_adoption_events(
                    &events,
                    RuntimeAdoptionReviewFilters {
                        track: track
                            .as_ref()
                            .map(runtime_adoption_track_slug)
                            .map(str::to_string),
                        feature,
                        signal: signal
                            .as_ref()
                            .map(runtime_adoption_signal_slug)
                            .map(str::to_string),
                        limit,
                    },
                );
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: Some(report.into()),
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "readiness" => {
                let db = self.open_db()?;
                let candidate = required_string(request.candidate.as_deref(), "candidate")?;
                let report = match candidate {
                    "card-context-default" => {
                        let events = db
                            .list_runtime_adoption_events(
                                &RuntimeAdoptionFilter {
                                    track: Some(RuntimeAdoptionTrack::CardContext),
                                    feature: Some("include_cards".to_string()),
                                },
                                10_000,
                            )
                            .map_err(|error| {
                                ErrorData::internal_error(
                                    format!("failed to list runtime adoption events: {error}"),
                                    None,
                                )
                            })?;
                        card_context_default_readiness(&events)
                    }
                    other => {
                        return Err(ErrorData::invalid_params(
                            format!("unsupported phase3 readiness candidate: {other}"),
                            None,
                        ));
                    }
                };
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: Some(report.into()),
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "record" => {
                let db = self.open_db()?;
                let track = parse_runtime_adoption_track(required_string(
                    request.track.as_deref(),
                    "track",
                )?)?;
                let signal = parse_runtime_adoption_signal(required_string(
                    request.signal.as_deref(),
                    "signal",
                )?)?;
                let feature = required_string(request.feature.as_deref(), "feature")?.to_string();
                let event = RuntimeAdoptionEvent {
                    id: request
                        .id
                        .unwrap_or_else(|| phase3_event_id(&track, &signal, &feature)),
                    track,
                    signal,
                    feature,
                    query: trim_to_owned(request.query.as_deref()),
                    context_hash: trim_to_owned(request.context_hash.as_deref()),
                    card_id: trim_to_owned(request.card_id.as_deref()),
                    evaluator_id: trim_to_owned(request.evaluator_id.as_deref()),
                    research_report_id: trim_to_owned(request.research_report_id.as_deref()),
                    note: trim_to_owned(request.note.as_deref()),
                    metadata: request.metadata,
                    created_at: current_timestamp(),
                };
                db.insert_runtime_adoption_event(&event).map_err(|error| {
                    ErrorData::internal_error(
                        format!("failed to insert runtime adoption event: {error}"),
                        None,
                    )
                })?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: Some(RuntimeAdoptionEventDto::from(event)),
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "list" => {
                let db = self.open_db()?;
                let events = db
                    .list_runtime_adoption_events(
                        &RuntimeAdoptionFilter {
                            track: parse_runtime_adoption_track_opt(request.track.as_deref())?,
                            feature: trim_to_owned(request.feature.as_deref()),
                        },
                        request.limit.unwrap_or(50),
                    )
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("failed to list runtime adoption events: {error}"),
                            None,
                        )
                    })?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: events
                        .into_iter()
                        .map(RuntimeAdoptionEventDto::from)
                        .collect(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "stats" => {
                let db = self.open_db()?;
                let events = db
                    .list_runtime_adoption_events(
                        &RuntimeAdoptionFilter {
                            track: parse_runtime_adoption_track_opt(request.track.as_deref())?,
                            feature: trim_to_owned(request.feature.as_deref()),
                        },
                        10_000,
                    )
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("failed to list runtime adoption events: {error}"),
                            None,
                        )
                    })?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: Some(runtime_adoption_stats(&events)),
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "analytics" => {
                let db = self.open_db()?;
                let events = db
                    .list_runtime_adoption_events(&RuntimeAdoptionFilter::default(), 10_000)
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("failed to list runtime adoption events: {error}"),
                            None,
                        )
                    })?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: Some(build_runtime_adoption_analytics(&events).into()),
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "gate" => {
                let db = self.open_db()?;
                let candidate = required_string(request.candidate.as_deref(), "candidate")?;
                let gate = phase3_gate_report(&db, candidate)?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: Some(gate),
                    research_plan: None,
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "research_validate_plan" => {
                let report = request.report.ok_or_else(|| {
                    ErrorData::invalid_params("report is required for research_validate_plan", None)
                })?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: Some(validate_research_adapter_plan_value(&report)),
                    research_ingest_plan: None,
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            "research_ingest_plan" => {
                let report = request.report.ok_or_else(|| {
                    ErrorData::invalid_params("report is required for research_ingest_plan", None)
                })?;
                Ok(Json(Phase3Response {
                    guidance: None,
                    instrumentation_policy: None,
                    record_plan: None,
                    record_quality: None,
                    record_checked: None,
                    review_report: None,
                    readiness_report: None,
                    event: None,
                    events: Vec::new(),
                    stats: None,
                    analytics: None,
                    gate: None,
                    research_plan: None,
                    research_ingest_plan: Some(ResearchIngestPlanDto::from(
                        build_research_ingest_plan_from_value(&report),
                    )),
                    evaluator_advice: None,
                    default_proposal: None,
                    rollback_control: None,
                }))
            }
            other => Err(ErrorData::invalid_params(
                format!(
                    "unsupported phase3 action: {other}; actions are guidance, instrumentation_policy, prepare_record, capture, evaluator_advise, default_proposal, rollback_control, check_record, record_checked, review, readiness, record, list, stats, gate, research_validate_plan, research_ingest_plan"
                ),
                None,
            )),
        }
    }
}

fn skill_to_dto(skill: &crate::core::skills::Skill) -> SkillDto {
    SkillDto {
        skill_id: skill.skill_id.clone(),
        name: skill.name.clone(),
        trigger_description: skill.trigger_description.clone(),
        pattern_id: skill.pattern_id.clone(),
        exemplar_ids: skill.exemplar_ids.clone(),
        adoption_count: skill.adoption_count,
        rejection_count: skill.rejection_count,
        eta: skill.eta(),
        status: skill.status.as_str().to_string(),
        promoted_at_unix_ms: skill.promoted_at,
        updated_at_unix_ms: skill.updated_at,
        project_id: skill.project_id.clone(),
    }
}

fn required_bus_field(
    value: Option<String>,
    field: &str,
    action: &str,
) -> std::result::Result<String, ErrorData> {
    value.ok_or_else(|| {
        ErrorData::invalid_params(format!("action `{action}` requires `{field}`"), None)
    })
}

fn bus_error_to_mcp(error: BusError) -> ErrorData {
    match error {
        BusError::InvalidAgentId(_)
        | BusError::InvalidTool(_)
        | BusError::UnsupportedTransport(_)
        | BusError::InvalidChannel(_)
        | BusError::InvalidThreadId(_)
        | BusError::UnknownChannel(_)
        | BusError::EmptyChannel(_)
        | BusError::TmuxTargetRequired
        | BusError::TmuxFailed(_)
        | BusError::TmuxCaptureFailed(_)
        | BusError::TmuxProbeFailed(_)
        | BusError::NotTmuxAgent(_)
        | BusError::InvalidLineCount(_)
        | BusError::InvalidSessionId(_)
        | BusError::EmptySession(_)
        | BusError::UnknownSession(_)
        | BusError::InvalidSessionStatus(_)
        | BusError::UnsupportedCaptureSource(_)
        | BusError::MissingCaptureDatabase
        | BusError::InvalidTimestamp(_)
        | BusError::UnknownSource(_)
        | BusError::UnknownTarget(_)
        | BusError::UnknownAgent(_)
        | BusError::UnknownDelivery(_)
        | BusError::DeliveryTargetMismatch { .. }
        | BusError::CannotAckFailed(_)
        | BusError::SelfSend(_)
        | BusError::MessageTooLarge(_)
        | BusError::InboxFull { .. } => ErrorData::invalid_params(error.to_string(), None),
        BusError::LegacyInbox(_) | BusError::Io(_) | BusError::Json(_) | BusError::Db(_) => {
            ErrorData::internal_error(error.to_string(), None)
        }
    }
}

fn agent_record_to_dto(record: AgentRecord) -> CoworkBusAgentDto {
    CoworkBusAgentDto {
        agent_id: record.agent_id,
        tool: record.tool,
        transport: record.transport,
        tmux_target: record.tmux_target,
        registered_at: record.registered_at,
        updated_at: record.updated_at,
        presence: if record.last_seen_at.is_some() {
            "online".to_string()
        } else {
            "never_seen".to_string()
        },
        last_seen_at: record.last_seen_at,
        pending_count: 0,
        pending_bytes: 0,
    }
}

fn agent_status_to_dto(status: AgentStatus) -> CoworkBusAgentDto {
    CoworkBusAgentDto {
        agent_id: status.record.agent_id,
        tool: status.record.tool,
        transport: status.record.transport,
        tmux_target: status.record.tmux_target,
        registered_at: status.record.registered_at,
        updated_at: status.record.updated_at,
        last_seen_at: status.record.last_seen_at,
        presence: status.presence,
        pending_count: status.pending_count,
        pending_bytes: status.pending_bytes,
    }
}

fn delivery_to_dto(delivery: DeliveryReport) -> CoworkBusDeliveryDto {
    CoworkBusDeliveryDto {
        message_id: delivery.message_id,
        target_agent_id: delivery.target_agent_id,
        transport: delivery.transport,
        inbox_path: delivery
            .inbox_path
            .map(|path| path.to_string_lossy().to_string()),
        inbox_size_after: delivery.inbox_size_after,
        tmux_target: delivery.tmux_target,
        thread_id: delivery.thread_id,
        channel: delivery.channel,
    }
}

fn delivery_status_to_dto(
    status: crate::cowork::bus::DeliveryStatus,
) -> CoworkBusDeliveryStatusDto {
    CoworkBusDeliveryStatusDto {
        message_id: status.message_id,
        event_type: status.event_type,
        status: status.status,
        from: status.from,
        target_agent_id: status.target_agent_id,
        transport: status.transport,
        message_preview: status.message_preview,
        thread_id: status.thread_id,
        channel: status.channel,
        delivered_at: status.delivered_at,
        updated_at: status.updated_at,
        acked_by: status.acked_by,
    }
}

fn message_to_dto(message: InboxMessage) -> CoworkBusMessageDto {
    CoworkBusMessageDto {
        pushed_at: message.pushed_at,
        from: message.from,
        content: message.content,
        thread_id: message.thread_id,
        channel: message.channel,
    }
}

fn event_to_dto(event: crate::cowork::bus::BusEvent) -> CoworkBusEventDto {
    CoworkBusEventDto {
        event_id: event.event_id,
        occurred_at: event.occurred_at,
        event_type: event.event_type,
        status: event.status,
        actor_agent_id: event.actor_agent_id,
        target_agent_ids: event.target_agent_ids,
        transport: event.transport,
        message_preview: event.message_preview,
        thread_id: event.details.get("thread_id").cloned(),
        channel: event.details.get("channel").cloned(),
        details: event.details,
    }
}

fn channel_to_dto(channel: crate::cowork::bus::ChannelRecord) -> CoworkBusChannelDto {
    CoworkBusChannelDto {
        channel: channel.channel,
        agents: channel.agents,
        updated_at: channel.updated_at,
    }
}

fn tmux_peek_to_dto(peek: crate::cowork::bus::TmuxPeek) -> CoworkBusTmuxPeekDto {
    CoworkBusTmuxPeekDto {
        agent_id: peek.agent_id,
        tmux_target: peek.tmux_target,
        lines: peek.lines,
        content: peek.content,
    }
}

fn doctor_to_dto(report: crate::cowork::bus::DoctorReport) -> CoworkBusDoctorDto {
    CoworkBusDoctorDto {
        status: report.status,
        agent_count: report.agent_count,
        channel_count: report.channel_count,
        session_count: report.session_count,
        stale_agents: report.stale_agents,
        never_seen_agents: report.never_seen_agents,
        pending_deliveries: report.pending_deliveries,
        warnings: report.warnings,
        tmux: report
            .tmux
            .into_iter()
            .map(|probe| CoworkBusTmuxProbeDto {
                agent_id: probe.agent_id,
                tmux_target: probe.tmux_target,
                status: probe.status,
                detail: probe.detail,
            })
            .collect(),
    }
}

fn session_to_dto(session: crate::cowork::bus::TeamSession) -> CoworkBusSessionDto {
    CoworkBusSessionDto {
        session_id: session.session_id,
        title: session.title,
        goal: session.goal,
        agents: session.agents,
        channels: session.channels,
        thread_id: session.thread_id,
        status: session.status,
        created_at: session.created_at,
        updated_at: session.updated_at,
    }
}

fn handoff_to_dto(summary: crate::cowork::bus::HandoffSummary) -> CoworkBusHandoffDto {
    CoworkBusHandoffDto {
        filters: CoworkBusHandoffFiltersDto {
            thread_id: summary.filters.thread_id,
            channel: summary.filters.channel,
            session_id: summary.filters.session_id,
            limit: summary.filters.limit,
        },
        sessions: summary.sessions.into_iter().map(session_to_dto).collect(),
        agents: summary
            .agents
            .into_iter()
            .map(|agent| CoworkBusHandoffAgentDto {
                agent_id: agent.agent_id,
                tool: agent.tool,
                presence: agent.presence,
                pending_count: agent.pending_count,
            })
            .collect(),
        pending_deliveries: summary
            .pending_deliveries
            .into_iter()
            .map(delivery_status_to_dto)
            .collect(),
        recent_events: summary
            .recent_events
            .into_iter()
            .map(event_to_dto)
            .collect(),
    }
}

fn capture_to_dto(report: crate::cowork::bus::CoworkCaptureReport) -> CoworkBusCaptureDto {
    CoworkBusCaptureDto {
        writes: report.writes,
        drawer_id: report.drawer_id,
        wing: report.wing,
        room: report.room,
        source: report.source,
        content: report.content,
    }
}

/// Return the current UTC timestamp in RFC 3339 format (seconds precision).
fn current_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = now;
    crate::cowork::peek::format_rfc3339(UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MempalMcpServer {
    fn get_info(&self) -> ServerInfo {
        let config = ConfigHandle::current();
        let progressive_disclosure_active = config.search.progressive_disclosure;
        let mut instructions = crate::core::protocol::MEMORY_PROTOCOL.to_string();
        if progressive_disclosure_active {
            instructions.push_str(
                "\n\nRULE 10 (progressive disclosure): When progressive disclosure is active, mempal_search returns truncated previews and still includes content_truncated plus original_content_bytes on every result. Use mempal_read_drawer or mempal_read_drawers to fetch full verbatim content after you decide which drawer merits a deeper read. For narrow queries, pass disable_progressive=true on mempal_search to request verbatim content directly.",
            );
        }
        if global_embed_status().is_degraded() {
            instructions.push_str(
                "\n\n11a. DEGRADED EMBED BACKEND\nWhen system_warnings mention an embed degradation, stop write operations and use read-only tools until recovery.",
            );
        }
        let mut experimental = BTreeMap::new();
        experimental.insert(
            "mempal".to_string(),
            serde_json::Map::from_iter([(
                "progressive_disclosure_active".to_string(),
                serde_json::Value::Bool(progressive_disclosure_active),
            )]),
        );
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        capabilities.experimental = Some(experimental);

        let mut info = ServerInfo::default();
        info.capabilities = capabilities;
        info.instructions = Some(instructions);
        info
    }

    async fn initialize(
        &self,
        request: rmcp::model::InitializeRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::InitializeResult, ErrorData> {
        if let Ok(mut guard) = self.client_name.lock() {
            *guard = Some(request.client_info.name.clone());
        }
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request.clone());
        }

        if let Ok(mut guard) = self.client_project_id.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = self.client_peer.lock() {
            *guard = Some(context.peer.clone());
        }

        Ok(self.get_info())
    }

    async fn on_roots_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::RoleServer>,
    ) {
        if let Ok(mut guard) = self.client_project_id.lock() {
            *guard = None;
        }
    }
}

pub(super) fn db_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(format!("{error}"), None)
}

fn replacement_db_error(error: crate::core::db::DbError) -> ErrorData {
    match error {
        crate::core::db::DbError::ReplacementTargetConflict
        | crate::core::db::DbError::SupersededDrawerNotFound { .. }
        | crate::core::db::DbError::SupersededDrawerProjectMismatch { .. }
        | crate::core::db::DbError::ReplacementTextNotFound
        | crate::core::db::DbError::ReplacementTextAmbiguous { .. } => {
            ErrorData::invalid_params(error.to_string(), None)
        }
        _ => db_error(error),
    }
}

fn exact_duplicate_drawer_id(
    db: &Database,
    content: &str,
    wing: &str,
    room: Option<&str>,
    project_id: Option<&str>,
    excluded_drawer_id: Option<&str>,
    metadata: &ValidatedIngestMetadata,
) -> std::result::Result<Option<String>, ErrorData> {
    let candidates = db
        .find_active_drawers_by_content(content, wing, room, project_id)
        .map_err(db_error)?
        .into_iter()
        .filter(|summary| Some(summary.id.as_str()) != excluded_drawer_id)
        .collect::<Vec<_>>();

    for candidate in candidates {
        let Some(drawer) = db.get_drawer(&candidate.id).map_err(db_error)? else {
            continue;
        };
        if drawer_matches_ingest_metadata(&drawer, metadata) {
            return Ok(Some(candidate.id));
        }
    }

    Ok(None)
}

fn supersede_drawer_for_ingest(
    db: &Database,
    old_id: &str,
    new_id: &str,
) -> std::result::Result<(), ErrorData> {
    db.supersede_drawer(old_id, &format!("replaced by {new_id}"))
        .map_err(db_error)?;
    Ok(())
}

fn drawer_matches_ingest_metadata(drawer: &Drawer, metadata: &ValidatedIngestMetadata) -> bool {
    drawer.memory_kind == metadata.memory_kind
        && drawer.domain == metadata.domain
        && drawer.field == metadata.field
        && drawer.anchor_kind == metadata.anchor_kind
        && drawer.anchor_id == metadata.anchor_id
        && drawer.parent_anchor_id == metadata.parent_anchor_id
        && drawer.is_pinned == metadata.is_pinned
        && drawer.provenance == metadata.provenance
        && drawer.statement == metadata.statement
        && drawer.tier == metadata.tier
        && drawer.status == metadata.status
        && normalized_string_sets_match(&drawer.supporting_refs, &metadata.supporting_refs)
        && normalized_string_sets_match(&drawer.counterexample_refs, &metadata.counterexample_refs)
        && normalized_string_sets_match(&drawer.teaching_refs, &metadata.teaching_refs)
        && normalized_string_sets_match(&drawer.verification_refs, &metadata.verification_refs)
        && drawer.scope_constraints == metadata.scope_constraints
        && trigger_hints_match(&drawer.trigger_hints, &metadata.trigger_hints)
}

fn normalized_string_sets_match(left: &[String], right: &[String]) -> bool {
    fn normalize(values: &[String]) -> Vec<String> {
        let mut normalized = values
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>();
        normalized.sort();
        normalized
    }

    normalize(left) == normalize(right)
}

fn trigger_hints_match(left: &Option<TriggerHints>, right: &Option<TriggerHints>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            normalized_string_sets_match(&left.intent_tags, &right.intent_tags)
                && normalized_string_sets_match(&left.workflow_bias, &right.workflow_bias)
                && normalized_string_sets_match(&left.tool_needs, &right.tool_needs)
        }
        _ => false,
    }
}

#[allow(dead_code)]
fn ingest_error(error: IngestError) -> ErrorData {
    match error {
        IngestError::DiaryRollupWrongWing { .. }
        | IngestError::DiaryRollupMissingRoom
        | IngestError::DailyRollupFull { .. } => ErrorData::invalid_params(error.to_string(), None),
        _ => ErrorData::internal_error(error.to_string(), None),
    }
}

fn fact_check_error(error: crate::factcheck::FactCheckError) -> ErrorData {
    match error {
        crate::factcheck::FactCheckError::InvalidScope(_)
        | crate::factcheck::FactCheckError::InvalidNow(_) => {
            ErrorData::invalid_params(error.to_string(), None)
        }
        crate::factcheck::FactCheckError::Db(_) => {
            ErrorData::internal_error(format!("fact_check: {error}"), None)
        }
    }
}

fn knowledge_gate_error(error: anyhow::Error) -> ErrorData {
    ErrorData::invalid_params(error.to_string(), None)
}

fn knowledge_distill_error(error: anyhow::Error) -> ErrorData {
    let message = error.to_string();
    if message.contains("failed to embed")
        || message.contains("failed to insert")
        || message.contains("failed to append audit")
        || message.contains("embedder required")
    {
        return ErrorData::internal_error(message, None);
    }
    ErrorData::invalid_params(message, None)
}

fn knowledge_lifecycle_error(error: anyhow::Error) -> ErrorData {
    let message = error.to_string();
    if message.contains("failed to update")
        || message.contains("failed to append audit")
        || message.contains("failed to open audit")
        || message.contains("failed to write audit")
    {
        return ErrorData::internal_error(message, None);
    }
    ErrorData::invalid_params(message, None)
}

fn knowledge_card_lifecycle_error(error: anyhow::Error) -> ErrorData {
    let message = error.to_string();
    if message.contains("failed to update")
        || message.contains("failed to insert")
        || message.contains("failed to append")
        || message.contains("failed to list")
    {
        return ErrorData::internal_error(message, None);
    }
    ErrorData::invalid_params(message, None)
}

fn knowledge_anchor_error(error: anyhow::Error) -> ErrorData {
    let message = error.to_string();
    if message.contains("failed to update")
        || message.contains("failed to append audit")
        || message.contains("failed to open audit")
        || message.contains("failed to write audit")
    {
        return ErrorData::internal_error(message, None);
    }
    ErrorData::invalid_params(message, None)
}

fn context_error(error: crate::context::ContextError) -> ErrorData {
    match error {
        crate::context::ContextError::DeriveAnchor(_) => {
            ErrorData::invalid_params(error.to_string(), None)
        }
        crate::context::ContextError::EmbedQuery(_)
        | crate::context::ContextError::MissingQueryVector
        | crate::context::ContextError::Search(_)
        | crate::context::ContextError::LoadDrawer(_)
        | crate::context::ContextError::LoadCard(_)
        | crate::context::ContextError::Foresight(_)
        | crate::context::ContextError::Tiered(_) => {
            ErrorData::internal_error(format!("context assembly failed: {error}"), None)
        }
    }
}

fn parse_context_trigger(s: &str) -> crate::search::tiered::ContextTrigger {
    match s {
        "on_demand" => crate::search::tiered::ContextTrigger::OnDemand,
        "repair" => crate::search::tiered::ContextTrigger::Repair,
        _ => crate::search::tiered::ContextTrigger::SessionStart,
    }
}

fn ensure_vector_dim_matches(
    db: &Database,
    actual_dim: usize,
) -> std::result::Result<(), ErrorData> {
    let Some(current_dim) = current_vector_dim(db).map_err(db_error)? else {
        return Ok(());
    };
    if current_dim == actual_dim {
        return Ok(());
    }
    Err(ErrorData::internal_error(
        format!(
            "embedding dimension mismatch: drawer_vectors uses {current_dim}d but embedder returned {actual_dim}d; run `mempal reindex --embedder <name>` before ingesting more content"
        ),
        None,
    ))
}

fn current_vector_dim(
    db: &Database,
) -> std::result::Result<Option<usize>, crate::core::db::DbError> {
    use rusqlite::OptionalExtension;

    let exists: bool = db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='drawer_vectors')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }

    let dimension = db
        .conn()
        .query_row(
            "SELECT vec_length(embedding) FROM drawer_vectors LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .map(|value| value as usize);
    Ok(dimension)
}

fn degraded_write_error() -> ErrorData {
    let warnings = current_system_warnings();
    let message = "mempal embed backend degraded; writes are paused until recovery. Read operations remain available.";
    let data = Some(serde_json::json!({
        "reason": "embed_degraded",
        "action": "write_refused",
        "system_warnings": warnings,
    }));
    ErrorData::internal_error(message, data)
}

fn maybe_database_write_refused_error(
    db_path: &Path,
    stage: &str,
    error: &(dyn std::error::Error + 'static),
) -> Option<ErrorData> {
    let diagnostic = status_database_diagnostic(db_path, stage, error);
    if diagnostic.failure_kind == "unknown" {
        return None;
    }
    Some(database_write_refused_error_from_diagnostic(diagnostic))
}

fn database_write_refused_error(
    db_path: &Path,
    stage: &str,
    error: &(dyn std::error::Error + 'static),
) -> ErrorData {
    let diagnostic = status_database_diagnostic(db_path, stage, error);
    database_write_refused_error_from_diagnostic(diagnostic)
}

fn database_write_refused_error_from_diagnostic(diagnostic: DatabaseDiagnosticDto) -> ErrorData {
    let warning = SystemWarning {
        level: "warn".to_string(),
        message: format!(
            "database diagnostic degraded at {}: {} ({})",
            diagnostic.source, diagnostic.summary, diagnostic.failure_kind
        ),
        source: "database".to_string(),
    };
    let message = format!(
        "mempal database degraded; writes are refused to preserve memory integrity. {}: {} ({}). {}",
        diagnostic.source, diagnostic.summary, diagnostic.failure_kind, diagnostic.hint
    );
    ErrorData::internal_error(
        message,
        Some(serde_json::json!({
            "reason": "database_degraded",
            "action": "write_refused",
            "database_diagnostic": diagnostic,
            "system_warnings": [warning],
        })),
    )
}

pub(super) fn current_system_warnings() -> Vec<SystemWarning> {
    let mut warnings = global_embed_status()
        .collect_warnings()
        .into_iter()
        .map(|warning| SystemWarning {
            level: warning.level.to_string(),
            message: warning.message,
            source: warning.source.to_string(),
        })
        .collect::<Vec<_>>();
    warnings.extend(
        ConfigHandle::collect_runtime_warnings()
            .into_iter()
            .map(|warning| SystemWarning {
                level: warning.level.to_string(),
                message: warning.message,
                source: warning.source.to_string(),
            }),
    );
    warnings
}

fn push_model_backend_warnings(
    system_warnings: &mut Vec<SystemWarning>,
    config: &Config,
    endpoint_health: &crate::endpoint_health::EndpointHealthSnapshot,
    queue_stats: &crate::core::queue::QueueStats,
) {
    if config
        .ingest_gating
        .llm_judge
        .as_ref()
        .is_some_and(|judge| judge.enabled)
        && !endpoint_health.llm.reachable
    {
        system_warnings.push(SystemWarning {
            level: "error".to_string(),
            message: "LLM memory judge is configured but no chat-completion endpoint is reachable; memory quality gating is unavailable until generation recovers.".to_string(),
            source: "llm_generation".to_string(),
        });
    }
    if queue_stats.pending > 0 && !endpoint_health.embedding.reachable {
        system_warnings.push(SystemWarning {
            level: "warn".to_string(),
            message: "embedding queue has pending work and the embedding endpoint is unreachable; accepted queued writes will retry when an endpoint recovers.".to_string(),
            source: "embed".to_string(),
        });
    }
    if queue_stats.failed > 0 {
        system_warnings.push(SystemWarning {
            level: "warn".to_string(),
            message: format!(
                "queue has failed work (retryable_model={}, terminal={}); retryable model tasks are auto-requeued when their endpoint recovers, terminal failures require manual action.",
                queue_stats.failed_retryable, queue_stats.failed_terminal
            ),
            source: "queue".to_string(),
        });
    }
}

fn format_deadline(deadline: Duration) -> String {
    let millis = deadline.as_millis();
    if millis >= 1_000 && millis % 1_000 == 0 {
        format!("{}s", deadline.as_secs())
    } else {
        format!("{millis}ms")
    }
}

fn mcp_stage_timeout_warning(stage: &str, deadline: Duration) -> String {
    format!(
        "{stage} exceeded {}; returning a bounded diagnostic before the MCP client timeout",
        format_deadline(deadline)
    )
}

fn mcp_stage_timeout_error(operation: &str, stage: &str, deadline: Duration) -> ErrorData {
    ErrorData::internal_error(
        format!(
            "{operation} {stage} exceeded {}; no durable result was confirmed before returning",
            format_deadline(deadline)
        ),
        None,
    )
}

fn push_mcp_timeout_warning(
    response_warnings: &mut Vec<String>,
    system_warnings: &mut Vec<SystemWarning>,
    stage: &str,
    deadline: Duration,
) {
    let warning = mcp_stage_timeout_warning(stage, deadline);
    response_warnings.push(warning.clone());
    system_warnings.push(SystemWarning {
        level: "warn".to_string(),
        message: warning,
        source: "mcp_timeout".to_string(),
    });
}

fn push_mcp_search_database_warning(
    response_warnings: &mut Vec<String>,
    system_warnings: &mut Vec<SystemWarning>,
    db_path: &Path,
    stage: &str,
    error: &(dyn std::error::Error + 'static),
) -> bool {
    let diagnostic = status_database_diagnostic(db_path, stage, error);
    if diagnostic.failure_kind == "unknown" {
        return false;
    }

    let warning = format!(
        "database diagnostic degraded at {}: {} ({}); mempal_search returned a bounded empty response instead of an internal MCP error. {}",
        diagnostic.source, diagnostic.summary, diagnostic.failure_kind, diagnostic.hint
    );
    response_warnings.push(warning.clone());
    system_warnings.push(SystemWarning {
        level: "warn".to_string(),
        message: warning,
        source: "database".to_string(),
    });
    true
}

fn push_db_holder_warnings(
    system_warnings: &mut Vec<SystemWarning>,
    report: &crate::process_diagnostics::DbHolderReport,
) {
    if let Some(error) = report.error.as_deref() {
        system_warnings.push(SystemWarning {
            level: "warn".to_string(),
            message: format!("database holder process inspection failed: {error}"),
            source: "db_holders".to_string(),
        });
    }
    if report.stale_mcp_server_count > 0 {
        system_warnings.push(SystemWarning {
            level: "warn".to_string(),
            message: format!(
                "{} stale mempal MCP server process(es) hold palace.db open",
                report.stale_mcp_server_count
            ),
            source: "db_holders".to_string(),
        });
    }
    if report.orphan_daemon_count > 0 {
        system_warnings.push(SystemWarning {
            level: "warn".to_string(),
            message: format!(
                "{} orphan daemon process(es) hold palace.db open",
                report.orphan_daemon_count
            ),
            source: "db_holders".to_string(),
        });
    }
    if report.extra_holder_count > 0 {
        system_warnings.push(SystemWarning {
            level: "warn".to_string(),
            message: format!(
                "{} extra process(es) hold palace.db open",
                report.extra_holder_count
            ),
            source: "db_holders".to_string(),
        });
    }
}

fn stale_index_warning_from_bool(is_stale: bool) -> Option<SystemWarning> {
    is_stale.then(|| SystemWarning {
        level: "warn".to_string(),
        message: "drawer_vectors index is stale (metric mismatch: l2, expected cosine); vector recall is degraded; run `mempal reindex --from-config --stale` to rebuild".to_string(),
        source: "vector_index".to_string(),
    })
}

fn stale_index_warning(db: &Database) -> Option<SystemWarning> {
    stale_index_warning_from_bool(db.vector_index_is_stale().unwrap_or(false))
}

fn system_warnings_with_stale_index(db: &Database) -> Vec<SystemWarning> {
    let mut warnings = current_system_warnings();
    if let Some(warning) = stale_index_warning(db) {
        warnings.push(warning);
    }
    warnings
}

fn operation_record_accepted_at_secs(record: &crate::core::queue::PendingOperationRecord) -> i64 {
    if record.completed_at.is_some() {
        record.created_at.div_euclid(1_000)
    } else {
        record.created_at
    }
}

fn queue_wait_ms(created_at_secs: i64, claimed_at_secs: i64) -> u64 {
    claimed_at_secs
        .saturating_sub(created_at_secs)
        .max(0)
        .saturating_mul(1_000) as u64
}

async fn complete_failed_ingest_claim(
    queue: &AsyncPendingMessageStore,
    claim: &ClaimedMessage,
    queue_wait_ms: u64,
    detail: String,
) -> anyhow::Result<()> {
    let mut finalized =
        finalize_failed_ingest_response(claim.id.clone(), claim.created_at, detail.clone());
    finalized
        .timings
        .insert("queue_wait_ms".to_string(), queue_wait_ms);
    let result_json =
        serde_json::to_string(&finalized).context("failed to serialize failed ingest response")?;
    queue
        .complete_operation(
            claim.clone(),
            IngestOperationState::Failed.as_str().to_string(),
            None,
            None,
            Some(detail),
            Some(result_json),
        )
        .await
        .map_err(|error| anyhow::anyhow!("failed to store async ingest failure: {error}"))?;
    Ok(())
}

fn rfc3339_from_secs(secs: i64) -> String {
    crate::cowork::peek::format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs.max(0) as u64))
}

fn finalize_ingest_response(
    operation_id: String,
    accepted_at_secs: i64,
    mut response: IngestResponse,
    state: IngestOperationState,
    rejected_reason: Option<String>,
    failure_detail: Option<String>,
) -> IngestResponse {
    response.operation_id = Some(operation_id);
    response.accepted_at = Some(rfc3339_from_secs(accepted_at_secs));
    response.state = Some(state);
    response.rejected_reason = rejected_reason;
    response.failure_detail = failure_detail;
    response
}

fn finalize_failed_ingest_response(
    operation_id: String,
    accepted_at_secs: i64,
    failure_detail: String,
) -> IngestResponse {
    finalize_ingest_response(
        operation_id,
        accepted_at_secs,
        IngestResponse {
            operation_id: None,
            accepted_at: None,
            state: None,
            timed_out: false,
            drawer_id: String::new(),
            drawer_ids: Vec::new(),
            chunk_count: 0,
            dropped: false,
            gating_decision: None,
            novelty_action: None,
            near_drawer_id: None,
            duplicate_warning: None,
            lock_wait_ms: None,
            superseded_drawer_id: None,
            rejected_reason: None,
            failure_detail: None,
            timings: BTreeMap::new(),
            fact_check_warnings: Vec::new(),
            system_warnings: current_system_warnings(),
        },
        IngestOperationState::Failed,
        None,
        Some(failure_detail),
    )
}

fn operation_record_to_response(
    record: crate::core::queue::PendingOperationRecord,
    system_warnings: Vec<SystemWarning>,
) -> IngestResponse {
    let accepted_at = Some(rfc3339_from_secs(operation_record_accepted_at_secs(
        &record,
    )));
    let state = record
        .op_state
        .parse::<IngestOperationState>()
        .unwrap_or(IngestOperationState::Failed);

    if let Some(result_json) = record.result_json.as_deref()
        && let Ok(mut response) = serde_json::from_str::<IngestResponse>(result_json)
    {
        response.operation_id = Some(record.id);
        response.accepted_at = accepted_at;
        response.state = Some(state);
        response.dropped = matches!(state, IngestOperationState::Rejected);
        if !matches!(state, IngestOperationState::Completed) {
            response.drawer_id.clear();
            response.drawer_ids.clear();
        }
        response.rejected_reason = record.rejected_reason.or(response.rejected_reason);
        response.failure_detail = record.failure_detail.or(response.failure_detail);
        response.system_warnings = system_warnings;
        return response;
    }

    IngestResponse {
        operation_id: Some(record.id),
        accepted_at,
        state: Some(state),
        timed_out: false,
        drawer_id: if matches!(state, IngestOperationState::Completed) {
            record.result_drawer_id.unwrap_or_default()
        } else {
            String::new()
        },
        drawer_ids: Vec::new(),
        chunk_count: 0,
        dropped: matches!(state, IngestOperationState::Rejected),
        gating_decision: None,
        novelty_action: None,
        near_drawer_id: None,
        duplicate_warning: None,
        lock_wait_ms: None,
        superseded_drawer_id: None,
        rejected_reason: record.rejected_reason,
        failure_detail: record.failure_detail,
        timings: BTreeMap::new(),
        fact_check_warnings: Vec::new(),
        system_warnings,
    }
}

fn intelligence_llm_state(config: &crate::core::config::Config, reachable: bool) -> String {
    if !config.memory_intelligence.mode.uses_llm() {
        return "disabled".to_string();
    }
    if !config
        .memory_intelligence
        .has_effective_llm_endpoint(&config.llm)
    {
        return "disabled".to_string();
    }
    if reachable {
        "healthy".to_string()
    } else {
        "degraded".to_string()
    }
}

fn read_drawer_response(details: crate::core::types::DrawerDetails) -> ReadDrawerResponse {
    let signals = crate::aaak::analyze(&details.drawer.content);
    let original_content_bytes = details.drawer.content.len() as u64;
    let vector = details.vector;
    let drawer = details.drawer;
    ReadDrawerResponse {
        drawer_id: drawer.id.clone(),
        content: drawer.content,
        content_truncated: false,
        original_content_bytes,
        wing: drawer.wing,
        room: drawer.room,
        source_file: source_file_or_synthetic(&drawer.id, drawer.source_file.as_deref()),
        created_at: drawer.added_at,
        updated_at: details.updated_at,
        merge_count: details.merge_count,
        importance_stars: signals.importance_stars,
        has_vector: vector.has_vector,
        vector_dimension: vector.dimension,
        vector_embedder: vector.embedder,
        vector_model: vector.model,
        vector_embedder_fingerprint: vector.embedder_fingerprint,
        vector_index_version: vector.index_version,
        vector_current_embedder_fingerprint: vector.current_embedder_fingerprint,
        vector_current_index_version: vector.current_index_version,
        vector_distance_metric: vector.distance_metric,
        vector_stale: vector.stale,
    }
}

const DEDUP_THRESHOLD: f32 = 0.85;

fn check_semantic_duplicate(
    db: &Database,
    vector: &[f32],
    _content: &str,
) -> Option<DuplicateWarning> {
    use crate::core::types::RouteDecision;

    let route = RouteDecision {
        wing: None,
        room: None,
        confidence: 0.0,
        reason: "dedup check".to_string(),
    };
    let scope = ProjectSearchScope::all_projects();
    let results = crate::search::search_by_vector(db, vector, route, &scope, 1).ok()?;
    let top = results.first()?;
    if top.similarity >= DEDUP_THRESHOLD {
        Some(DuplicateWarning {
            similar_drawer_id: top.drawer_id.clone(),
            similarity: top.similarity,
            preview: top.content.chars().take(100).collect(),
        })
    } else {
        None
    }
}

fn triple_to_dto(triple: &Triple) -> TripleDto {
    TripleDto {
        id: triple.id.clone(),
        subject: triple.subject.clone(),
        predicate: triple.predicate.clone(),
        object: triple.object.clone(),
        valid_from: triple.valid_from.clone(),
        valid_to: triple.valid_to.clone(),
        confidence: triple.confidence,
        source_drawer: triple.source_drawer.clone(),
    }
}

fn passive_tunnel_dtos(
    db: &Database,
    wing: Option<&str>,
) -> std::result::Result<Vec<TunnelDto>, ErrorData> {
    let wing = wing.map(str::trim).filter(|value| !value.is_empty());
    let tunnels = db
        .find_tunnels()
        .map_err(db_error)?
        .into_iter()
        .filter(|(_, wings)| wing.is_none_or(|filter| wings.iter().any(|item| item == filter)))
        .map(|(room, wings)| TunnelDto {
            tunnel_id: passive_tunnel_id(&room),
            kind: "passive".to_string(),
            room: Some(room),
            wings,
            left: None,
            right: None,
            label: None,
            created_at: None,
            created_by: None,
            via_tunnel_id: None,
            hop: None,
        })
        .collect();
    Ok(tunnels)
}

fn explicit_tunnel_to_dto(tunnel: &ExplicitTunnel) -> TunnelDto {
    TunnelDto {
        tunnel_id: tunnel.id.clone(),
        kind: "explicit".to_string(),
        room: None,
        wings: vec![tunnel.left.wing.clone(), tunnel.right.wing.clone()],
        left: Some(TunnelEndpointDto::from(&tunnel.left)),
        right: Some(TunnelEndpointDto::from(&tunnel.right)),
        label: Some(tunnel.label.clone()),
        created_at: Some(tunnel.created_at.clone()),
        created_by: tunnel.created_by.clone(),
        via_tunnel_id: None,
        hop: None,
    }
}

fn passive_tunnel_id(room: &str) -> String {
    let sanitized = room
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("passive_{sanitized}")
}
#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use rusqlite::params;
    use tempfile::TempDir;
    use tokio::sync::Notify;

    use super::*;
    use crate::core::db::read_fork_ext_version;
    use crate::core::types::BootstrapEvidenceArgs;
    use crate::core::types::{KnowledgeCard, KnowledgeEvidenceLink, KnowledgeEvidenceRole};
    use crate::embed::Embedder;

    #[derive(Clone)]
    struct StubEmbedderFactory {
        vector: Vec<f32>,
    }

    struct StubEmbedder {
        vector: Vec<f32>,
    }

    #[derive(Clone)]
    struct BlockingEmbedderFactory {
        vector: Vec<f32>,
        call_count: Arc<AtomicUsize>,
        gate: Arc<Notify>,
    }

    struct BlockingEmbedder {
        vector: Vec<f32>,
        call_count: Arc<AtomicUsize>,
        gate: Arc<Notify>,
    }

    struct EmbedStatusResetGuard;

    impl Drop for EmbedStatusResetGuard {
        fn drop(&mut self) {
            global_embed_status().reset_for_tests();
        }
    }

    #[derive(Default)]
    struct KnowledgeRefs {
        supporting: Vec<String>,
        counterexample: Vec<String>,
        teaching: Vec<String>,
        verification: Vec<String>,
    }

    struct KnowledgeAnchorArgs<'a> {
        domain: MemoryDomain,
        anchor_kind: AnchorKind,
        anchor_id: &'a str,
        parent_anchor_id: Option<&'a str>,
    }

    #[async_trait]
    impl crate::embed::EmbedderFactory for StubEmbedderFactory {
        async fn build(&self) -> crate::embed::Result<Box<dyn Embedder>> {
            Ok(Box::new(StubEmbedder {
                vector: self.vector.clone(),
            }))
        }
    }

    #[async_trait]
    impl Embedder for StubEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.vector.clone()).collect())
        }

        fn dimensions(&self) -> usize {
            self.vector.len()
        }

        fn name(&self) -> &str {
            "stub"
        }
    }

    #[async_trait]
    impl crate::embed::EmbedderFactory for BlockingEmbedderFactory {
        async fn build(&self) -> crate::embed::Result<Box<dyn Embedder>> {
            Ok(Box::new(BlockingEmbedder {
                vector: self.vector.clone(),
                call_count: Arc::clone(&self.call_count),
                gate: Arc::clone(&self.gate),
            }))
        }
    }

    #[async_trait]
    impl Embedder for BlockingEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            self.gate.notified().await;
            Ok(texts.iter().map(|_| self.vector.clone()).collect())
        }

        fn dimensions(&self) -> usize {
            self.vector.len()
        }

        fn name(&self) -> &str {
            "blocking-stub"
        }
    }

    fn setup_server() -> (TempDir, PathBuf, MempalMcpServer) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db fixture");
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
        .expect("create MCP server")
        .with_async_db_for_test(async_db);
        (tempdir, db_path, server)
    }

    fn assert_json_string_values_do_not_contain(value: &Value, needle: &str, rendered: &str) {
        match value {
            Value::String(text) => assert!(!text.contains(needle), "{rendered}"),
            Value::Array(items) => {
                for item in items {
                    assert_json_string_values_do_not_contain(item, needle, rendered);
                }
            }
            Value::Object(fields) => {
                for field_value in fields.values() {
                    assert_json_string_values_do_not_contain(field_value, needle, rendered);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    #[tokio::test]
    async fn test_mcp_status_redacts_blocked_remote_endpoint_identity() {
        global_embed_status().reset_for_tests();
        let _embed_status_guard = EmbedStatusResetGuard;
        let config = Config::parse(
            r#"
[privacy.remote_calls]
fail_closed = true

[embed]
backend = "openai_compat"
base_url = "https://api.openai.com:9443/v1/private-embed-path"
api_model = "text-embedding-3-large"

[embed.openai_compat]
api_key_env = "MEMPAL_SECRET_TOKEN_ENV"

[llm]
enabled = true
base_url = "https://llm.example.com/v1/private-chat-path"
model = "judge"
api_key = "sk-secret-should-not-print"
"#,
        )
        .expect("parse config");
        let stale_error = crate::embed::EmbedError::Runtime(
            "failed https://api.openai.com:9443/v1/private-embed-path?api_key=sk-secret-should-not-print MEMPAL_SECRET_TOKEN_ENV"
                .to_string(),
        );
        global_embed_status().record_endpoint_cooldown(
            "legacy",
            Duration::from_secs(30),
            &stale_error,
        );
        global_embed_status().record_failure(&stale_error);
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db fixture");
        let server = MempalMcpServer::new_with_factory_and_config(
            db_path,
            config,
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
        .expect("create MCP server")
        .with_async_db_for_test(async_db);

        let status = server.mempal_status().await.expect("status").0;
        let rendered_value = serde_json::to_value(&status).expect("serialize status");
        let rendered = serde_json::to_string(&rendered_value).expect("serialize status");

        assert_eq!(
            status.embed_status.base_url.as_deref(),
            Some(crate::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL)
        );
        assert!(
            status
                .embed_status
                .endpoints
                .iter()
                .all(|endpoint| endpoint.base_url
                    == crate::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL),
            "{rendered}"
        );
        assert!(
            status
                .llm_status
                .endpoints
                .iter()
                .all(|endpoint| endpoint.base_url
                    == crate::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL),
            "{rendered}"
        );
        assert!(rendered.contains("skipped"), "{rendered}");
        assert!(status.embed_status.last_error.is_none(), "{rendered}");
        assert!(
            status
                .embed_status
                .endpoints
                .iter()
                .all(|endpoint| endpoint.last_error.is_none()),
            "{rendered}"
        );
        assert!(
            rendered.contains("privacy.remote_calls.fail_closed"),
            "{rendered}"
        );
        assert!(rendered.contains("allow_embedding"), "{rendered}");
        assert!(rendered.contains("allow_llm"), "{rendered}");
        assert!(!rendered.contains("api.openai.com"), "{rendered}");
        assert!(!rendered.contains("llm.example.com"), "{rendered}");
        assert!(!rendered.contains("private-embed-path"), "{rendered}");
        assert!(!rendered.contains("private-chat-path"), "{rendered}");
        assert!(
            !rendered.contains("sk-secret-should-not-print"),
            "{rendered}"
        );
        assert!(!rendered.contains("MEMPAL_SECRET_TOKEN_ENV"), "{rendered}");
        assert_json_string_values_do_not_contain(&rendered_value, "9443", &rendered);
        assert_json_string_values_do_not_contain(&rendered_value, "api_key", &rendered);
    }

    #[test]
    fn test_mcp_invalid_source_type_error_does_not_echo_raw_value() {
        let raw = "private-invalid-source-type";
        let error = parse_source_type_param(Some(raw)).expect_err("source_type should reject raw");
        let message = format!("{error:?}");

        assert!(message.contains("source_type must be one of"));
        assert!(
            !message.contains(raw),
            "invalid source_type errors must not echo caller-supplied raw values"
        );
    }

    #[tokio::test]
    async fn test_mcp_explicit_ingest_bypasses_llm_gating() {
        let _config_guard = ConfigOverrideGuard::install(
            r#"
[llm]
enabled = true
base_url = "http://127.0.0.1:9/v1"
model = "unreachable-test-llm"
enabled_for = ["gating"]

[gating]
enabled = true

[gating.embedding_classifier]
enabled = true
threshold = 0.5
prototypes = ["keep"]

[gating.llm_judge]
enabled = true
quality_policy = "llm_required_for_keep"
"#,
        )
        .await;
        let (_tempdir, db_path, server) = setup_server();

        let response = server
            .mempal_ingest(Parameters(IngestRequest {
                content: "Explicit MCP agent write must bypass LLM filtering even when automatic hook filtering is required.".to_string(),
                wing: "mcp".to_string(),
                room: Some("explicit".to_string()),
                source_type: Some("agent_inference".to_string()),
                wait: Some(true),
                wait_timeout_secs: Some(5),
                ..IngestRequest::default()
            }))
            .await
            .expect("explicit MCP ingest should not depend on LLM availability")
            .0;

        assert!(!response.dropped);
        assert_eq!(response.state, Some(IngestOperationState::Completed));
        assert!(!response.drawer_id.is_empty());
        assert!(
            response.gating_decision.as_ref().is_none_or(|decision| {
                decision.label.as_deref() != Some("llm_pending")
                    && decision.label.as_deref() != Some("llm_keep")
            }),
            "explicit MCP ingest must not enter the LLM filtering path: {:?}",
            response.gating_decision
        );
        let queue = PendingMessageStore::new_without_reclaim(&db_path);
        assert!(
            queue
                .claim_next_by_kind("mcp-explicit-bypass", 60, "llm_task")
                .expect("claim llm task")
                .is_none(),
            "explicit MCP ingest must not enqueue post-insert LLM filtering tasks"
        );
        let audit_label: Option<String> = Database::open(&db_path)
            .expect("open db")
            .conn()
            .query_row(
                "SELECT label FROM gating_audit WHERE candidate_hash = ?1",
                [response.drawer_id.as_str()],
                |row| row.get(0),
            )
            .expect("query gating audit label");
        assert_ne!(audit_label.as_deref(), Some("llm_pending"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_mcp_ingest_admission_prefers_daemon_ipc_queue() {
        let (tempdir, db_path, server) = setup_server();
        let (listener, _socket_guard) =
            crate::hook_ipc::bind_listener(tempdir.path()).expect("bind daemon IPC");
        let daemon = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept daemon IPC");
            let request = crate::hook_ipc::read_enqueue_request(&mut stream)
                .await
                .expect("read daemon IPC request");
            crate::hook_ipc::write_enqueue_response(
                &mut stream,
                &crate::hook_ipc::HookIpcEnqueueResponse::Accepted,
            )
            .await
            .expect("write daemon IPC response");
            request
        });

        let response = server
            .mempal_ingest(Parameters(IngestRequest {
                content: "daemon-owned MCP queue admission".to_string(),
                wing: "mcp".to_string(),
                room: Some("busy".to_string()),
                wait: Some(false),
                ..IngestRequest::default()
            }))
            .await
            .expect("ingest admission should succeed")
            .0;

        let request = daemon.await.expect("daemon IPC task");
        assert_eq!(request.kind, INGEST_ASYNC_KIND);
        let operation_id = response.operation_id.as_deref().expect("operation id");
        assert_eq!(
            operation_id,
            PendingMessageStore::idempotent_message_id(INGEST_ASYNC_KIND, &request.idempotency_key)
        );
        let local_record = PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(operation_id)
            .expect("query local queue");
        assert!(
            local_record.is_none(),
            "MCP admission should not write the local queue when daemon IPC accepts"
        );
    }

    #[tokio::test]
    async fn test_mcp_doctor_missing_db_is_read_only_after_server_construction() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_dir = tempdir.path().join("missing-home").join(".mempal");
        let db_path = db_dir.join("palace.db");
        assert!(!db_dir.exists());

        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
        .expect("create MCP server");
        assert!(
            !db_dir.exists(),
            "server construction must not create db dir"
        );

        let response = server
            .mempal_doctor(Parameters(DoctorRequest {}))
            .await
            .expect("doctor")
            .0;

        assert_eq!(response.db.path, db_path.display().to_string());
        assert!(!response.db.exists);
        assert_eq!(response.db.schema_version, None);
        assert!(response.db.compatible);
        assert!(!db_path.exists(), "doctor must not create database file");
        assert!(
            !db_dir.exists(),
            "doctor must not create database directory"
        );
    }

    fn knowledge_card(
        id: &str,
        tier: KnowledgeTier,
        status: KnowledgeStatus,
        field: &str,
    ) -> KnowledgeCard {
        KnowledgeCard {
            id: id.to_string(),
            statement: format!("Statement for {id}."),
            content: format!("Content for {id}."),
            tier,
            status,
            domain: MemoryDomain::Project,
            field: field.to_string(),
            anchor_kind: AnchorKind::Repo,
            anchor_id: "repo://mempal".to_string(),
            parent_anchor_id: None,
            scope_constraints: Some("Only for MCP read tests.".to_string()),
            trigger_hints: Some(TriggerHints {
                intent_tags: vec!["memory".to_string()],
                workflow_bias: vec!["inspect-first".to_string()],
                tool_needs: vec!["mcp".to_string()],
            }),
            auto_generated: false,
            crystallization_score: None,
            source_drawer_ids: Vec::new(),
            created_at: "1713000000".to_string(),
            updated_at: "1713000000".to_string(),
        }
    }

    fn insert_knowledge_card(db_path: &Path, card: KnowledgeCard) {
        let db = Database::open(db_path).expect("open db");
        db.insert_knowledge_card(&card)
            .expect("insert knowledge card");
    }

    fn insert_knowledge_card_link(
        db_path: &Path,
        id: &str,
        card_id: &str,
        evidence_drawer_id: &str,
        role: KnowledgeEvidenceRole,
    ) {
        let db = Database::open(db_path).expect("open db");
        db.insert_knowledge_evidence_link(&KnowledgeEvidenceLink {
            id: id.to_string(),
            card_id: card_id.to_string(),
            evidence_drawer_id: evidence_drawer_id.to_string(),
            role,
            note: None,
            created_at: "1713000000".to_string(),
        })
        .expect("insert knowledge card link");
    }

    /// Serializes tests that override the global `ConfigHandle` snapshot and
    /// resets it to defaults on Drop so other parallel tests do not see leaked
    /// overrides.
    struct ConfigOverrideGuard {
        _lock: tokio::sync::OwnedMutexGuard<()>,
        _tempdir: TempDir,
    }

    impl ConfigOverrideGuard {
        async fn install(toml_contents: &str) -> Self {
            let lock = crate::core::config::global_config_test_lock()
                .lock_owned()
                .await;
            let tempdir = tempfile::tempdir().expect("config override tempdir");
            let path = tempdir.path().join("override.toml");
            fs::write(&path, toml_contents).expect("write config override");
            crate::core::config::ConfigHandle::harness_reload_from_path(&path);
            Self {
                _lock: lock,
                _tempdir: tempdir,
            }
        }
    }

    impl Drop for ConfigOverrideGuard {
        fn drop(&mut self) {
            let tempdir = tempfile::tempdir().expect("config reset tempdir");
            let path = tempdir.path().join("default.toml");
            fs::write(&path, "").expect("write default config");
            crate::core::config::ConfigHandle::harness_reload_from_path(&path);
        }
    }

    fn insert_drawer(
        db_path: &Path,
        id: &str,
        content: &str,
        wing: &str,
        room: Option<&str>,
        source_file: &str,
        importance: i32,
    ) {
        let db = Database::open(db_path).expect("open db");
        db.insert_drawer(&Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: wing.to_string(),
            room: room.map(str::to_string),
            source_file: Some(source_file.to_string()),
            source_type: SourceType::AgentInference,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance,
        }))
        .expect("insert drawer");
        db.insert_vector(id, &[0.1, 0.2, 0.3])
            .expect("insert vector");
    }

    fn spawn_runtime_ticker() -> (Arc<AtomicU64>, tokio::task::JoinHandle<()>) {
        let ticks = Arc::new(AtomicU64::new(0));
        let ticks_bg = Arc::clone(&ticks);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                ticks_bg.fetch_add(1, Ordering::SeqCst);
            }
        });
        (ticks, ticker)
    }

    fn assert_runtime_ticked(ticks: &AtomicU64, label: &str) {
        let observed = ticks.load(Ordering::SeqCst);
        assert!(
            observed >= 5,
            "{label} advanced ticker {observed} times; MCP DB work must not block Tokio worker"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_status_db_work_runs_off_runtime() {
        let (_tempdir, db_path, server) = setup_server();
        let async_db = AsyncDb::open(&db_path, 4)
            .expect("open async db")
            .with_read_delay(Duration::from_millis(300));
        let server = server.with_async_db_for_test(async_db);
        let (ticks, ticker) = spawn_runtime_ticker();

        let status = server.mempal_status().await.expect("status").0;
        ticker.abort();

        assert!(status.schema_version > 0);
        assert_runtime_ticked(&ticks, "mempal_status");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_search_db_work_runs_off_runtime() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "offruntime-search",
            "offruntime search marker",
            "mcp",
            Some("runtime"),
            "offruntime-search.md",
            3,
        );
        let async_db = AsyncDb::open(&db_path, 4)
            .expect("open async db")
            .with_read_delay(Duration::from_millis(300));
        let server = server.with_async_db_for_test(async_db);
        let (ticks, ticker) = spawn_runtime_ticker();

        let response = server
            .mempal_search(Parameters(SearchRequest {
                query: "offruntime search marker".to_string(),
                wing: Some("mcp".to_string()),
                room: Some("runtime".to_string()),
                top_k: Some(1),
                ..SearchRequest::default()
            }))
            .await
            .expect("search")
            .0;
        ticker.abort();

        assert!(!response.results.is_empty());
        assert_runtime_ticked(&ticks, "mempal_search");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_search_returns_diagnostic_when_db_reads_exceed_deadline() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "bounded-search",
            "bounded search marker",
            "mcp",
            Some("deadline"),
            "bounded-search.md",
            3,
        );
        let async_db = AsyncDb::open(&db_path, 4)
            .expect("open async db")
            .with_read_delay(Duration::from_millis(150));
        let server = server
            .with_async_db_for_test(async_db)
            .with_mcp_deadline_for_test(Duration::from_millis(20));

        let response = tokio::time::timeout(
            Duration::from_millis(500),
            server.mempal_search(Parameters(SearchRequest {
                query: "bounded search marker".to_string(),
                wing: Some("mcp".to_string()),
                room: Some("deadline".to_string()),
                top_k: Some(1),
                disable_progressive: Some(true),
                ..SearchRequest::default()
            })),
        )
        .await
        .expect("MCP search should return before client timeout")
        .expect("bounded diagnostic response")
        .0;

        assert_eq!(response.search_mode, SearchMode::Bm25Only.as_str());
        assert!(response.results.is_empty());
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("hybrid search exceeded")),
            "hybrid timeout warning missing: {:?}",
            response.warnings
        );
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("BM25 fallback search exceeded")),
            "BM25 timeout warning missing: {:?}",
            response.warnings
        );
        assert!(
            response
                .system_warnings
                .iter()
                .any(|warning| warning.source == "mcp_timeout"),
            "system warning should expose bounded MCP timeout"
        );

        tokio::time::sleep(Duration::from_millis(180)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_ingest_admission_db_work_runs_off_runtime() {
        let (_tempdir, db_path, server) = setup_server();
        let async_db = AsyncDb::open(&db_path, 4)
            .expect("open async db")
            .with_read_delay(Duration::from_millis(300));
        let server = server.with_async_db_for_test(async_db);
        let (ticks, ticker) = spawn_runtime_ticker();

        let response = server
            .mempal_ingest(Parameters(IngestRequest {
                content: "offruntime ingest admission".to_string(),
                wing: "mcp".to_string(),
                room: Some("runtime".to_string()),
                dry_run: Some(false),
                wait: Some(false),
                ..IngestRequest::default()
            }))
            .await
            .expect("ingest")
            .0;
        ticker.abort();

        assert_eq!(response.state, Some(IngestOperationState::Queued));
        assert!(response.operation_id.is_some());
        assert_runtime_ticked(&ticks, "mempal_ingest admission");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_ingest_admission_returns_error_when_db_reads_exceed_deadline() {
        let (_tempdir, db_path, server) = setup_server();
        let async_db = AsyncDb::open(&db_path, 4)
            .expect("open async db")
            .with_read_delay(Duration::from_millis(150));
        let server = server
            .with_async_db_for_test(async_db)
            .with_mcp_deadline_for_test(Duration::from_millis(20));

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            server.mempal_ingest(Parameters(IngestRequest {
                content: "bounded ingest admission".to_string(),
                wing: "mcp".to_string(),
                room: Some("deadline".to_string()),
                dry_run: Some(false),
                wait: Some(false),
                ..IngestRequest::default()
            })),
        )
        .await
        .expect("MCP ingest should return before client timeout");
        let error = match result {
            Ok(_) => panic!("slow admission should not claim durable acceptance"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("mempal_ingest admission preparation exceeded"),
            "unexpected error: {error}"
        );

        tokio::time::sleep(Duration::from_millis(180)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_ingest_queue_admission_waits_for_receipt_after_deadline() {
        let (_tempdir, db_path, server) = setup_server();
        let async_queue = AsyncPendingMessageStore::new_without_reclaim(&db_path)
            .with_blocking_delay(Duration::from_millis(150));
        let server = server
            .with_async_queue_for_test(async_queue)
            .with_mcp_deadline_for_test(Duration::from_millis(20));

        let response = tokio::time::timeout(
            Duration::from_millis(500),
            server.mempal_ingest(Parameters(IngestRequest {
                content: "slow queue admission still returns a receipt".to_string(),
                wing: "mcp".to_string(),
                room: Some("receipt".to_string()),
                dry_run: Some(false),
                wait: Some(false),
                ..IngestRequest::default()
            })),
        )
        .await
        .expect("MCP ingest should wait for queue admission receipt")
        .expect("slow queue admission should still succeed")
        .0;

        assert_eq!(response.state, Some(IngestOperationState::Queued));
        let operation_id = response
            .operation_id
            .as_deref()
            .expect("queued response must include operation id");
        let record = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(operation_id)
            .expect("load queued operation status")
            .expect("operation must be durable when receipt is returned");
        assert_eq!(record.id, operation_id);
        assert_eq!(record.kind, INGEST_ASYNC_KIND);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_ingest_dry_run_sync_preview_runs_off_runtime() {
        let (_tempdir, db_path, server) = setup_server();
        let server = server.with_ingest_processing_delay_for_test(Duration::from_millis(300));
        let (ticks, ticker) = spawn_runtime_ticker();

        let response = server
            .mempal_ingest(Parameters(IngestRequest {
                content: "dry-run preview must not block the MCP runtime".to_string(),
                wing: "mcp".to_string(),
                room: Some("runtime".to_string()),
                dry_run: Some(true),
                wait: Some(false),
                ..IngestRequest::default()
            }))
            .await
            .expect("dry-run ingest")
            .0;
        ticker.abort();

        assert!(response.operation_id.is_none());
        assert!(response.state.is_none());
        assert_eq!(response.chunk_count, 1);
        assert!(!response.drawer_id.is_empty());
        assert_runtime_ticked(&ticks, "mempal_ingest dry-run preview");

        let db = Database::open(&db_path).expect("open db");
        let drawer_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
            .expect("count drawers");
        assert_eq!(drawer_count, 0, "dry-run ingest must not persist drawers");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_ingest_dry_run_returns_error_when_preview_exceeds_deadline() {
        let (_tempdir, _db_path, server) = setup_server();
        let server = server
            .with_ingest_processing_delay_for_test(Duration::from_millis(150))
            .with_mcp_deadline_for_test(Duration::from_millis(20));

        let result = tokio::time::timeout(
            Duration::from_millis(500),
            server.mempal_ingest(Parameters(IngestRequest {
                content: "bounded dry-run preview".to_string(),
                wing: "mcp".to_string(),
                room: Some("deadline".to_string()),
                dry_run: Some(true),
                wait: Some(false),
                ..IngestRequest::default()
            })),
        )
        .await
        .expect("MCP dry-run ingest should return before client timeout");
        let error = match result {
            Ok(_) => panic!("slow dry-run preview should not claim success"),
            Err(error) => error,
        };

        assert!(
            error
                .to_string()
                .contains("mempal_ingest dry-run admission exceeded"),
            "unexpected error: {error}"
        );

        tokio::time::sleep(Duration::from_millis(180)).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_ingest_drain_worker_runs_sync_ingest_off_runtime() {
        let (_tempdir, db_path, server) = setup_server();
        let server = server.with_ingest_processing_delay_for_test(Duration::from_millis(300));
        let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
        let request = IngestRequest {
            content: "offruntime queued ingest drain".to_string(),
            wing: "mcp".to_string(),
            room: Some("runtime".to_string()),
            project_id: Some("project-drain".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        };
        let project_id = server
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await
            .expect("resolve project");
        let prepared = server
            .prepare_async_ingest_operation(
                &request,
                IngestControls::default(),
                config.as_ref(),
                compiled_privacy.as_ref(),
                project_id,
            )
            .await
            .expect("prepare async ingest");
        let payload = serde_json::to_string(&prepared).expect("serialize prepared ingest");
        let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
        let operation_id = queue
            .enqueue(INGEST_ASYNC_KIND, &payload)
            .expect("enqueue async ingest");
        let claim = queue
            .claim_next_by_kind("worker-drain", 60, INGEST_ASYNC_KIND)
            .expect("claim queued op")
            .expect("claimed queued op");
        let async_queue = AsyncPendingMessageStore::from_store(queue.clone());
        let (ticks, ticker) = spawn_runtime_ticker();

        server
            .process_ingest_claim(&async_queue, "worker-drain", claim)
            .await
            .expect("process queued ingest");
        ticker.abort();

        assert_runtime_ticked(&ticks, "mempal_ingest drain worker");
        let completed = server
            .operation_status_json_for_test(&operation_id)
            .await
            .expect("completed status");
        assert_eq!(completed.state, Some(IngestOperationState::Completed));
        assert!(!completed.drawer_id.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_mcp_async_ingest_malformed_payload_records_failed_receipt() {
        let (_tempdir, db_path, server) = setup_server();
        let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
        let operation_id = queue
            .enqueue(INGEST_ASYNC_KIND, "{not json")
            .expect("enqueue malformed async ingest");
        let claim = queue
            .claim_next_by_kind("worker-malformed", 60, INGEST_ASYNC_KIND)
            .expect("claim malformed op")
            .expect("claimed malformed op");
        let async_queue = AsyncPendingMessageStore::from_store(queue.clone());

        server
            .process_ingest_claim(&async_queue, "worker-malformed", claim)
            .await
            .expect("process malformed payload");

        let failed = server
            .operation_status_json_for_test(&operation_id)
            .await
            .expect("failed status");
        assert_eq!(failed.state, Some(IngestOperationState::Failed));
        assert!(failed.drawer_id.is_empty());
        assert!(
            failed
                .failure_detail
                .as_deref()
                .is_some_and(|detail| detail.contains("failed to decode ingest operation")),
            "{failed:?}"
        );

        let record = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path)
            .operation_status(&operation_id)
            .expect("load operation record")
            .expect("operation record exists");
        assert_eq!(record.op_state, IngestOperationState::Failed.as_str());
        assert!(record.completed_at.is_some());
    }

    fn recreate_vectors_with_metric(db_path: &Path, metric: &str) {
        let db = Database::open(db_path).expect("open db");
        db.conn()
            .execute_batch(&format!(
                r#"
                DROP TABLE IF EXISTS drawer_vectors;
                CREATE VIRTUAL TABLE drawer_vectors USING vec0(
                    id TEXT PRIMARY KEY,
                    embedding FLOAT[3] distance_metric={metric},
                    +project_id TEXT
                );
                "#
            ))
            .expect("recreate vector table");
    }

    fn insert_test_vector(db_path: &Path, id: &str) {
        let db = Database::open(db_path).expect("open db");
        db.insert_vector_with_project(id, &[0.1, 0.2, 0.3], None)
            .expect("insert test vector");
    }

    fn has_vector_index_warning(warnings: &[SystemWarning]) -> bool {
        warnings.iter().any(|warning| {
            warning.source == "vector_index"
                && warning.message.contains("reindex --from-config --stale")
        })
    }

    #[test]
    fn test_db_holder_system_warnings_report_extra_holders_with_specific_roles() {
        let report = crate::process_diagnostics::DbHolderReport {
            db_path: "/tmp/palace.db".to_string(),
            holder_count: 3,
            extra_holder_count: 1,
            stale_mcp_server_count: 1,
            orphan_daemon_count: 1,
            error: None,
            holders: Vec::new(),
        };
        let mut warnings = Vec::new();

        push_db_holder_warnings(&mut warnings, &report);

        assert!(warnings.iter().any(|warning| {
            warning.source == "db_holders" && warning.message.contains("stale mempal MCP server")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.source == "db_holders" && warning.message.contains("orphan daemon")
        }));
        assert!(warnings.iter().any(|warning| {
            warning.source == "db_holders" && warning.message.contains("extra process")
        }));
    }

    fn insert_drawer_with_project(
        db_path: &Path,
        id: &str,
        wing: &str,
        room: Option<&str>,
        project_id: Option<&str>,
    ) {
        let db = Database::open(db_path).expect("open db");
        let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: format!("project scoped status fixture {id}"),
            wing: wing.to_string(),
            room: room.map(str::to_string),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::AgentInference,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance: 1,
        });
        db.insert_drawer_with_project(&drawer, project_id)
            .expect("insert project drawer");
    }

    fn scope_keys(status: &StatusResponse) -> Vec<(String, Option<String>)> {
        let mut keys = status
            .scopes
            .iter()
            .map(|scope| (scope.wing.clone(), scope.room.clone()))
            .collect::<Vec<_>>();
        keys.sort();
        keys
    }

    #[tokio::test]
    async fn test_mempal_status_compact_default_omits_protocol_but_keeps_health() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer_with_project(
            &db_path,
            "status-null-stale",
            "mempal",
            Some("status"),
            None,
        );
        let store = crate::core::queue::PendingMessageStore::with_config(
            &db_path,
            crate::core::queue::QueueConfig {
                base_delay_ms: 0,
                max_delay_ms: 0,
                max_retries: 0,
            },
        )
        .expect("create queue store");
        store
            .enqueue("hook_event", r#"{"status":true}"#)
            .expect("enqueue failed fixture");
        let failed = store
            .claim_next("status-worker", 60)
            .expect("claim failed fixture")
            .expect("failed fixture row");
        store
            .mark_failed_with_disposition(
                &failed,
                "boom",
                crate::core::queue::QueueFailureDisposition::Terminal,
            )
            .expect("mark terminal failed");

        let status = server.mempal_status().await.expect("status").0;
        let json = serde_json::to_value(&status).expect("serialize compact status");
        let fork_ext_version =
            read_fork_ext_version(Database::open(&db_path).expect("open db").conn())
                .expect("read fork ext version");

        assert!(json.get("memory_protocol").is_none());
        assert!(json.get("aaak_spec").is_none());
        assert!(json.get("schema_version").and_then(Value::as_u64).is_some());
        assert_eq!(
            json.get("fork_ext_version").and_then(Value::as_u64),
            Some(fork_ext_version.into())
        );
        assert_eq!(status.fork_ext_version, fork_ext_version);
        assert!(
            json.get("stale_drawer_count")
                .and_then(Value::as_u64)
                .is_some()
        );
        assert_eq!(status.queue_stats.failed, 1);
        assert_eq!(status.embed_status.failed_count, 1);
        assert!(json.get("endpoint_health").is_some());
        assert!(json.get("intelligence_status").is_some());
        assert!(
            status
                .system_warnings
                .iter()
                .any(|warning| warning.source == "project_isolation"),
            "compact status must keep actionable warnings"
        );

        let info = <MempalMcpServer as ServerHandler>::get_info(&server);
        let instructions = info.instructions.expect("server instructions");
        assert!(instructions.contains("MEMPAL MEMORY PROTOCOL"));
    }

    #[tokio::test]
    async fn test_mempal_status_full_includes_protocol_and_aaak() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer_with_project(
            &db_path,
            "status-full-null-warning",
            "mempal",
            Some("status"),
            None,
        );

        let status = server
            .mempal_status_with_options(StatusRequest {
                detail: Some(StatusDetail::Full),
                scope: Some(StatusScope::Project),
                project_id: None,
            })
            .await
            .expect("full status")
            .0;
        let json = serde_json::to_value(&status).expect("serialize full status");

        assert!(status.memory_protocol.contains("MEMPAL MEMORY PROTOCOL"));
        assert!(status.aaak_spec.contains("AAAK"));
        assert!(json.get("memory_protocol").is_some());
        assert!(json.get("aaak_spec").is_some());
        assert!(json.get("queue_stats").is_some());
        assert!(json.get("embed_status").is_some());
        assert!(json.get("stale_drawer_count").is_some());
        assert!(json.get("system_warnings").is_some());
    }

    #[test]
    fn test_status_database_diagnostic_classifies_sqlite_failures() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: rusqlite::ffi::SQLITE_BUSY,
            },
            Some("database is locked".to_string()),
        );
        let permission = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::PermissionDenied,
                extended_code: rusqlite::ffi::SQLITE_PERM,
            },
            Some("permission denied".to_string()),
        );
        let invalid = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::NotADatabase,
                extended_code: rusqlite::ffi::SQLITE_NOTADB,
            },
            Some("file is not a database".to_string()),
        );

        assert_eq!(status_db_failure_kind(&busy), "locked_or_busy");
        assert_eq!(status_db_failure_kind(&permission), "path_or_permission");
        assert_eq!(status_db_failure_kind(&invalid), "corrupt_or_invalid");
    }

    #[test]
    fn test_mcp_search_database_warning_classifies_sqlite_busy() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: rusqlite::ffi::SQLITE_BUSY,
            },
            Some("database is locked".to_string()),
        );
        let mut response_warnings = Vec::new();
        let mut system_warnings = Vec::new();

        let handled = push_mcp_search_database_warning(
            &mut response_warnings,
            &mut system_warnings,
            Path::new("/tmp/palace.db"),
            "BM25 fallback search",
            &busy,
        );

        assert!(handled);
        assert!(
            response_warnings
                .iter()
                .any(|warning| warning.contains("locked_or_busy")
                    && warning.contains("bounded empty response")),
            "response warning should expose locked DB diagnostic: {response_warnings:?}"
        );
        assert!(system_warnings.iter().any(|warning| {
            warning.source == "database" && warning.message.contains("locked_or_busy")
        }));
    }

    #[test]
    fn test_mcp_ingest_database_write_refused_error_classifies_sqlite_busy() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy,
                extended_code: rusqlite::ffi::SQLITE_BUSY,
            },
            Some("database is locked".to_string()),
        );

        let error = database_write_refused_error(Path::new("/tmp/palace.db"), "async_db", &busy);

        assert!(error.message.contains("writes are refused"));
        assert!(error.message.contains("locked_or_busy"));
        let data = error.data.expect("structured error data");
        assert_eq!(
            data.get("reason").and_then(Value::as_str),
            Some("database_degraded")
        );
        assert_eq!(
            data.get("action").and_then(Value::as_str),
            Some("write_refused")
        );
        let diagnostic = data
            .get("database_diagnostic")
            .expect("database diagnostic payload");
        assert_eq!(
            diagnostic.get("path").and_then(Value::as_str),
            Some("/tmp/palace.db")
        );
        assert_eq!(
            diagnostic.get("source").and_then(Value::as_str),
            Some("async_db")
        );
        assert_eq!(
            diagnostic.get("failure_kind").and_then(Value::as_str),
            Some("locked_or_busy")
        );
        assert!(
            diagnostic
                .get("summary")
                .and_then(Value::as_str)
                .is_some_and(|summary| summary.contains("database is locked")),
            "diagnostic summary should include the SQLite lock message: {diagnostic}"
        );
        assert_eq!(
            diagnostic.get("hint").and_then(Value::as_str),
            Some(
                "Check for stale daemon/MCP processes holding palace.db, wait for the writer to finish, then retry status."
            )
        );
        assert!(
            data.get("system_warnings")
                .and_then(Value::as_array)
                .is_some_and(|warnings| warnings.iter().any(|warning| {
                    warning.get("source").and_then(Value::as_str) == Some("database")
                        && warning
                            .get("message")
                            .and_then(Value::as_str)
                            .is_some_and(|message| message.contains("locked_or_busy"))
                })),
            "structured data should include database system warning: {data}"
        );
    }

    #[tokio::test]
    async fn test_mcp_search_returns_diagnostic_when_database_cannot_open() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        fs::create_dir(&db_path).expect("create directory at db path");
        let server = MempalMcpServer::new_with_factory(
            db_path,
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
        .expect("create MCP server");

        let response = server
            .mempal_search(Parameters(SearchRequest {
                query: "open failure marker".to_string(),
                top_k: Some(1),
                disable_progressive: Some(true),
                ..SearchRequest::default()
            }))
            .await
            .expect("search should return an actionable diagnostic response")
            .0;

        assert_eq!(response.search_mode, SearchMode::Bm25Only.as_str());
        assert!(response.results.is_empty());
        assert!(
            response.warnings.iter().any(|warning| {
                warning.contains("path_or_permission") && warning.contains("bounded empty response")
            }),
            "response warning should expose database diagnostic: {:?}",
            response.warnings
        );
        assert!(response.system_warnings.iter().any(|warning| {
            warning.source == "database" && warning.message.contains("path_or_permission")
        }));
    }

    #[tokio::test]
    async fn test_mempal_status_returns_diagnostic_when_database_cannot_open() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        fs::create_dir(&db_path).expect("create directory at db path");
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
        .expect("create MCP server");

        let status = server.mempal_status().await.expect("status").0;
        let diagnostic = status
            .database_diagnostic
            .expect("database diagnostic should be present");

        assert_eq!(diagnostic.path, db_path.display().to_string());
        assert_eq!(diagnostic.failure_kind, "path_or_permission");
        assert!(
            diagnostic.summary.contains("failed")
                || diagnostic.summary.contains("unable")
                || diagnostic.summary.contains("directory"),
            "diagnostic summary should include the underlying open failure: {}",
            diagnostic.summary
        );
        assert!(!diagnostic.hint.is_empty());
        assert_eq!(status.queue_stats.pending, 0);
        assert_eq!(status.queue_stats.claimed, 0);
        assert_eq!(status.queue_stats.failed, 0);
        assert!(status.system_warnings.iter().any(|warning| {
            warning.source == "database" && warning.message.contains("path_or_permission")
        }));
        assert!(
            server.open_db().is_err(),
            "diagnostic status must not make write/open paths succeed"
        );
    }

    #[tokio::test]
    async fn test_mcp_ingest_returns_structured_error_when_database_cannot_open() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        fs::create_dir(&db_path).expect("create directory at db path");
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
        .expect("create MCP server");

        let error = match server
            .mempal_ingest(Parameters(IngestRequest {
                content: "database-open failure should be transport-safe".to_string(),
                wing: "mcp".to_string(),
                room: Some("diagnostic".to_string()),
                dry_run: Some(false),
                wait: Some(true),
                wait_timeout_secs: Some(10),
                ..IngestRequest::default()
            }))
            .await
        {
            Ok(_) => panic!("ingest should return a structured MCP error"),
            Err(error) => error,
        };

        assert!(error.message.contains("writes are refused"));
        assert!(error.message.contains("path_or_permission"));
        let data = error.data.expect("structured error data");
        assert_eq!(
            data.get("reason").and_then(Value::as_str),
            Some("database_degraded")
        );
        assert_eq!(
            data.get("action").and_then(Value::as_str),
            Some("write_refused")
        );
        let diagnostic = data
            .get("database_diagnostic")
            .expect("database diagnostic payload");
        assert_eq!(
            diagnostic.get("path").and_then(Value::as_str),
            Some(db_path.display().to_string()).as_deref()
        );
        assert_eq!(
            diagnostic.get("failure_kind").and_then(Value::as_str),
            Some("path_or_permission")
        );
        assert!(
            data.get("system_warnings")
                .and_then(Value::as_array)
                .is_some_and(|warnings| warnings.iter().any(|warning| {
                    warning.get("source").and_then(Value::as_str) == Some("database")
                        && warning
                            .get("message")
                            .and_then(Value::as_str)
                            .is_some_and(|message| message.contains("path_or_permission"))
                })),
            "structured data should include database system warning: {data}"
        );
    }

    #[tokio::test]
    async fn test_mempal_status_scopes_project_by_default_and_all_on_opt_in() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer_with_project(
            &db_path,
            "status-project-a",
            "project-a-wing",
            Some("status"),
            Some("project-a"),
        );
        insert_drawer_with_project(
            &db_path,
            "status-project-b",
            "project-b-wing",
            Some("status"),
            Some("project-b"),
        );
        insert_drawer_with_project(
            &db_path,
            "status-global",
            "global-wing",
            Some("status"),
            None,
        );

        let project_status = server
            .mempal_status_with_options(StatusRequest {
                detail: None,
                scope: None,
                project_id: Some("project-a".to_string()),
            })
            .await
            .expect("project status")
            .0;
        assert_eq!(
            scope_keys(&project_status),
            vec![("project-a-wing".to_string(), Some("status".to_string()))]
        );

        let all_status = server
            .mempal_status_with_options(StatusRequest {
                detail: None,
                scope: Some(StatusScope::All),
                project_id: Some("project-a".to_string()),
            })
            .await
            .expect("all status")
            .0;
        let all_keys = scope_keys(&all_status);
        assert!(all_keys.contains(&("project-a-wing".to_string(), Some("status".to_string()))));
        assert!(all_keys.contains(&("project-b-wing".to_string(), Some("status".to_string()))));
        assert!(all_keys.contains(&("global-wing".to_string(), Some("status".to_string()))));

        let full_status = server
            .mempal_status_with_options(StatusRequest {
                detail: Some(StatusDetail::Full),
                scope: None,
                project_id: Some("project-a".to_string()),
            })
            .await
            .expect("full status")
            .0;
        assert!(
            scope_keys(&full_status)
                .contains(&("project-b-wing".to_string(), Some("status".to_string()))),
            "detail=full defaults to the CLI --full all-scope breakdown"
        );
    }

    #[tokio::test]
    async fn test_mempal_status_warns_when_vector_index_metric_is_stale() {
        let (_tempdir, db_path, server) = setup_server();
        recreate_vectors_with_metric(&db_path, "l2");

        let status = server.mempal_status().await.expect("status").0;

        assert!(status.vector_index_stale);
        assert!(has_vector_index_warning(&status.system_warnings));
    }

    #[tokio::test]
    async fn test_mempal_status_omits_vector_index_warning_when_metric_is_cosine() {
        let (_tempdir, db_path, server) = setup_server();
        recreate_vectors_with_metric(&db_path, "cosine");

        let status = server.mempal_status().await.expect("status").0;

        assert!(!status.vector_index_stale);
        assert!(!has_vector_index_warning(&status.system_warnings));
    }

    fn insert_knowledge_drawer(
        db_path: &Path,
        id: &str,
        tier: KnowledgeTier,
        status: KnowledgeStatus,
        statement: &str,
        content: &str,
    ) {
        insert_knowledge_drawer_with_refs(
            db_path,
            id,
            tier,
            status,
            statement,
            content,
            KnowledgeRefs {
                supporting: vec!["drawer_supporting_ev".to_string()],
                ..KnowledgeRefs::default()
            },
        );
    }

    fn insert_knowledge_drawer_with_refs(
        db_path: &Path,
        id: &str,
        tier: KnowledgeTier,
        status: KnowledgeStatus,
        statement: &str,
        content: &str,
        refs: KnowledgeRefs,
    ) {
        let db = Database::open(db_path).expect("open db");
        let source_type = SourceType::AgentInference;
        let drawer = Drawer {
            id: id.to_string(),
            content: content.to_string(),
            wing: "mempal".to_string(),
            room: Some("context".to_string()),
            source_file: Some(format!("knowledge://project/context/{id}")),
            source_type,
            confidence: crate::core::types::default_confidence(source_type),
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            normalize_version: 1,
            importance: 3,
            effective_importance: 3.0,
            memory_kind: MemoryKind::Knowledge,
            domain: MemoryDomain::Project,
            field: anchor::DEFAULT_FIELD.to_string(),
            anchor_kind: AnchorKind::Repo,
            anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
            parent_anchor_id: None,
            provenance: None,
            statement: Some(statement.to_string()),
            tier: Some(tier),
            status: Some(status),
            supporting_refs: refs.supporting,
            counterexample_refs: refs.counterexample,
            teaching_refs: refs.teaching,
            verification_refs: refs.verification,
            scope_constraints: None,
            trigger_hints: None,
            is_pinned: false,
            pin_order: None,
            supersedes: None,
            compacted_into: None,
        };
        db.insert_drawer(&drawer).expect("insert knowledge drawer");
        db.insert_vector(id, &[0.1, 0.2, 0.3])
            .expect("insert vector");
    }

    fn insert_knowledge_drawer_with_anchor(
        db_path: &Path,
        id: &str,
        status: KnowledgeStatus,
        anchor_args: KnowledgeAnchorArgs<'_>,
    ) {
        let db = Database::open(db_path).expect("open db");
        let source_type = SourceType::AgentInference;
        let drawer = Drawer {
            id: id.to_string(),
            content: format!("{id} content"),
            wing: "mempal".to_string(),
            room: Some("context".to_string()),
            source_file: Some(format!("knowledge://project/context/{id}")),
            source_type,
            confidence: crate::core::types::default_confidence(source_type),
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            normalize_version: 1,
            importance: 3,
            effective_importance: 3.0,
            memory_kind: MemoryKind::Knowledge,
            domain: anchor_args.domain,
            field: anchor::DEFAULT_FIELD.to_string(),
            anchor_kind: anchor_args.anchor_kind,
            anchor_id: anchor_args.anchor_id.to_string(),
            parent_anchor_id: anchor_args.parent_anchor_id.map(str::to_string),
            provenance: None,
            statement: Some(format!("{id} statement")),
            tier: Some(KnowledgeTier::Shu),
            status: Some(status),
            supporting_refs: vec!["drawer_supporting_ev".to_string()],
            counterexample_refs: Vec::new(),
            teaching_refs: Vec::new(),
            verification_refs: Vec::new(),
            scope_constraints: None,
            trigger_hints: None,
            is_pinned: false,
            pin_order: None,
            supersedes: None,
            compacted_into: None,
        };
        db.insert_drawer(&drawer)
            .expect("insert anchored knowledge drawer");
        db.insert_vector(id, &[0.1, 0.2, 0.3])
            .expect("insert anchored knowledge vector");
    }

    fn audit_line_count(db_path: &Path) -> usize {
        let audit_path = db_path
            .parent()
            .expect("db path has parent")
            .join("audit.jsonl");
        fs::read_to_string(audit_path)
            .map(|content| content.lines().count())
            .unwrap_or(0)
    }

    fn last_audit_entry(db_path: &Path) -> serde_json::Value {
        let audit_path = db_path
            .parent()
            .expect("db path has parent")
            .join("audit.jsonl");
        let content = fs::read_to_string(audit_path).expect("read audit log");
        serde_json::from_str(content.lines().last().expect("last audit line")).expect("audit json")
    }

    fn vector_row_count(db: &Database, id: &str) -> i64 {
        db.conn()
            .query_row(
                "SELECT COUNT(*) FROM drawer_vectors WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .expect("count vector rows")
    }

    fn total_vector_count(db: &Database) -> i64 {
        db.conn()
            .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| row.get(0))
            .expect("count vector rows")
    }

    fn insert_triple(
        db_path: &Path,
        subject: &str,
        predicate: &str,
        object: &str,
        valid_from: Option<&str>,
        valid_to: Option<&str>,
    ) {
        let db = Database::open(db_path).expect("open db");
        db.insert_triple(&Triple {
            id: crate::core::utils::build_triple_id(subject, predicate, object),
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            valid_from: valid_from.map(str::to_string),
            valid_to: valid_to.map(str::to_string),
            confidence: 1.0,
            source_drawer: None,
        })
        .expect("insert triple");
    }

    async fn run_search(
        server: &MempalMcpServer,
        query: &str,
        wing: Option<&str>,
        room: Option<&str>,
        top_k: usize,
    ) -> SearchResponse {
        server
            .mempal_search(Parameters(SearchRequest {
                query: query.to_string(),
                wing: wing.map(str::to_string),
                room: room.map(str::to_string),
                top_k: Some(top_k),
                memory_kind: None,
                domain: None,
                field: None,
                tier: None,
                status: None,
                anchor_kind: None,
                with_neighbors: None,
                project_id: None,
                include_global: None,
                all_projects: None,
                scope: None,
                disable_progressive: None,
                include_raw_turns: None,
                include_expired: None,
            }))
            .await
            .expect("search should succeed")
            .0
    }

    #[tokio::test]
    async fn test_mempal_search_includes_structured_signals_and_preserves_raw_fields() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer-1",
            "We decided to use Arc<Mutex<>> for state because shared ownership mattered",
            "mempal",
            Some("signals"),
            "/tmp/decision.md",
            4,
        );
        insert_drawer(
            &db_path,
            "drawer-2",
            "上海决定采用共享内存同步机制解决状态漂移问题",
            "mempal",
            Some("signals"),
            "/tmp/cjk.md",
            3,
        );

        let response = run_search(&server, "state", None, None, 2).await;

        assert_eq!(response.results.len(), 2);

        let decision = response
            .results
            .iter()
            .find(|result| result.drawer_id == "drawer-1")
            .expect("decision result");
        assert_eq!(
            decision.content,
            "We decided to use Arc<Mutex<>> for state because shared ownership mattered"
        );
        assert_eq!(decision.source_file, "/tmp/decision.md");
        assert!(decision.flags.contains(&"DECISION".to_string()));
        assert!(!decision.entities.is_empty());
        assert!(!decision.emotions.is_empty());
        assert!(decision.importance_stars >= 2);

        let cjk = response
            .results
            .iter()
            .find(|result| result.drawer_id == "drawer-2")
            .expect("cjk result");
        assert_ne!(cjk.entities, vec!["UNK".to_string()]);
    }

    #[tokio::test]
    async fn test_mempal_search_returns_empty_results_when_filters_exclude_all_drawers() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer-1",
            "We decided to use Arc<Mutex<>> for state because shared ownership mattered",
            "mempal",
            Some("signals"),
            "/tmp/decision.md",
            4,
        );

        let response = run_search(&server, "state", Some("other-wing"), None, 5).await;

        assert!(response.results.is_empty());
    }

    #[tokio::test]
    async fn test_mempal_search_warns_when_vector_index_metric_is_stale() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer-1",
            "stale vector metric search fixture",
            "mempal",
            Some("signals"),
            "/tmp/stale.md",
            4,
        );
        recreate_vectors_with_metric(&db_path, "l2");
        insert_test_vector(&db_path, "drawer-1");

        let response = run_search(&server, "stale vector metric", None, None, 5).await;

        assert_eq!(response.search_mode, "hybrid");
        assert!(has_vector_index_warning(&response.system_warnings));
    }

    #[tokio::test]
    async fn test_mempal_search_omits_vector_index_warning_when_metric_is_cosine() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer-1",
            "fresh vector metric search fixture",
            "mempal",
            Some("signals"),
            "/tmp/fresh.md",
            4,
        );

        let response = run_search(&server, "fresh vector metric", None, None, 5).await;

        assert!(!has_vector_index_warning(&response.system_warnings));
    }

    #[tokio::test]
    async fn test_mempal_search_has_no_db_side_effects() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer-1",
            "We decided to use Arc<Mutex<>> for state because shared ownership mattered",
            "mempal",
            Some("signals"),
            "/tmp/decision.md",
            4,
        );

        let db = Database::open(&db_path).expect("open db");
        let baseline_drawers = db.drawer_count().expect("drawer count");
        let baseline_triples = db.triple_count().expect("triple count");
        let baseline_schema = db.schema_version().expect("schema version");

        for _ in 0..3 {
            let response = run_search(&server, "state", None, None, 5).await;
            assert!(!response.results.is_empty());
        }

        let db = Database::open(&db_path).expect("reopen db");
        assert_eq!(db.drawer_count().expect("drawer count"), baseline_drawers);
        assert_eq!(db.triple_count().expect("triple count"), baseline_triples);
        assert_eq!(
            db.schema_version().expect("schema version"),
            baseline_schema
        );
    }

    #[tokio::test]
    async fn test_mcp_context_returns_tier_ordered_sections() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer(
            &db_path,
            "drawer_qi",
            KnowledgeTier::Qi,
            KnowledgeStatus::Promoted,
            "Use cargo test.",
            "debug failing build qi",
        );
        insert_knowledge_drawer(
            &db_path,
            "drawer_shu",
            KnowledgeTier::Shu,
            KnowledgeStatus::Promoted,
            "Reproduce before patching.",
            "debug failing build shu",
        );
        insert_knowledge_drawer(
            &db_path,
            "drawer_dao_ren",
            KnowledgeTier::DaoRen,
            KnowledgeStatus::Promoted,
            "Software changes need executable feedback.",
            "debug failing build dao ren",
        );
        insert_knowledge_drawer(
            &db_path,
            "drawer_dao_tian",
            KnowledgeTier::DaoTian,
            KnowledgeStatus::Canonical,
            "Evidence precedes assertion.",
            "debug failing build dao tian",
        );

        let response = server
            .context_json_for_test(serde_json::json!({
                "query": "debug failing build"
            }))
            .await
            .expect("context should succeed");
        let names = response
            .sections
            .iter()
            .map(|section| section.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["dao_tian", "dao_ren", "shu", "qi"]);
        for section in response.sections {
            assert_eq!(section.items.len(), 1);
            assert!(!section.items[0].drawer_id.is_empty());
            assert!(!section.items[0].source_file.is_empty());
        }
    }

    #[tokio::test]
    async fn test_mcp_context_defaults_match_cli_context_defaults() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer(
            &db_path,
            "drawer_shu",
            KnowledgeTier::Shu,
            KnowledgeStatus::Promoted,
            "Debug by reproducing.",
            "debug default body",
        );

        let response = server
            .context_json_for_test(serde_json::json!({ "query": "debug" }))
            .await
            .expect("context should succeed");
        assert_eq!(response.domain, "project");
        assert_eq!(response.field, "general");
        assert!(!response.anchors.is_empty());
        assert!(
            response
                .sections
                .iter()
                .all(|section| section.name != "evidence")
        );
        assert_eq!(response.sections[0].name, "shu");
        assert_eq!(response.sections[0].items[0].drawer_id, "drawer_shu");
    }

    #[tokio::test]
    async fn test_mcp_context_include_evidence_appends_evidence_section() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer(
            &db_path,
            "drawer_qi",
            KnowledgeTier::Qi,
            KnowledgeStatus::Promoted,
            "Use cargo test.",
            "observed failure qi",
        );
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "observed failure",
            "mempal",
            Some("context"),
            "/tmp/evidence.md",
            2,
        );

        let response = server
            .context_json_for_test(serde_json::json!({
                "query": "observed failure",
                "include_evidence": true
            }))
            .await
            .expect("context should succeed");
        let names = response
            .sections
            .iter()
            .map(|section| section.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["qi", "evidence"]);
        assert_eq!(response.sections[1].items[0].drawer_id, "drawer_evidence");
    }

    #[tokio::test]
    async fn test_mcp_context_include_cards_omitted_uses_config_default() {
        let _guard =
            ConfigOverrideGuard::install("[context]\ninclude_cards_default = true\n").await;
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_default_card_evidence",
            "default card evidence",
            "mempal",
            Some("context"),
            "/tmp/default-card-evidence.md",
            2,
        );
        let mut card = knowledge_card(
            "card_default_context",
            KnowledgeTier::Shu,
            KnowledgeStatus::Promoted,
            "general",
        );
        card.anchor_id = anchor::LEGACY_REPO_ANCHOR_ID.to_string();
        insert_knowledge_card(&db_path, card);
        insert_knowledge_card_link(
            &db_path,
            "link_card_default_context",
            "card_default_context",
            "drawer_default_card_evidence",
            KnowledgeEvidenceRole::Supporting,
        );

        let response = server
            .context_json_for_test(serde_json::json!({ "query": "default card" }))
            .await
            .expect("context should succeed");
        let card_item = response
            .sections
            .iter()
            .flat_map(|section| section.items.iter())
            .find(|item| item.card_id.as_deref() == Some("card_default_context"))
            .expect("card item should appear when include_cards omitted and config default true");
        assert_eq!(card_item.drawer_id, "card_default_context");
    }

    #[tokio::test]
    async fn test_mcp_context_dao_tian_limit_zero_omits_section() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer(
            &db_path,
            "drawer_dao_tian",
            KnowledgeTier::DaoTian,
            KnowledgeStatus::Canonical,
            "Evidence precedes assertion.",
            "debug universal principle",
        );
        insert_knowledge_drawer(
            &db_path,
            "drawer_shu",
            KnowledgeTier::Shu,
            KnowledgeStatus::Promoted,
            "Reproduce before patching.",
            "debug workflow rule",
        );

        let response = server
            .context_json_for_test(serde_json::json!({
                "query": "debug",
                "dao_tian_limit": 0
            }))
            .await
            .expect("context should succeed");
        let names = response
            .sections
            .iter()
            .map(|section| section.name.as_str())
            .collect::<Vec<_>>();
        assert!(!names.contains(&"dao_tian"));
        assert!(names.contains(&"shu"));
    }

    #[tokio::test]
    async fn test_mcp_context_rejects_max_items_zero() {
        let (_tempdir, _db_path, server) = setup_server();
        let error = server
            .context_json_for_test(serde_json::json!({
                "query": "debug",
                "max_items": 0
            }))
            .await
            .expect_err("max_items=0 should reject");
        assert!(error.to_string().contains("max_items"));
    }

    #[tokio::test]
    async fn test_mcp_context_rejects_unsupported_domain() {
        let (_tempdir, _db_path, server) = setup_server();
        let error = server
            .context_json_for_test(serde_json::json!({
                "query": "debug",
                "domain": "invalid"
            }))
            .await
            .expect_err("invalid domain should reject");
        assert!(error.to_string().contains("domain"));
    }

    #[tokio::test]
    async fn test_mcp_context_has_no_db_side_effects() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer(
            &db_path,
            "drawer_shu",
            KnowledgeTier::Shu,
            KnowledgeStatus::Promoted,
            "Debug by reproducing.",
            "debug side-effect body",
        );

        let db = Database::open(&db_path).expect("open db");
        let baseline_schema = db.schema_version().expect("schema");
        let baseline_drawers = db.drawer_count().expect("drawers");
        let baseline_triples = db.triple_count().expect("triples");
        let baseline_taxonomy = db.taxonomy_count().expect("taxonomy");
        let baseline_scopes = db.scope_counts().expect("scopes");

        for _ in 0..3 {
            let response = server
                .context_json_for_test(serde_json::json!({ "query": "debug" }))
                .await
                .expect("context should succeed");
            assert!(!response.sections.is_empty());
        }

        let db = Database::open(&db_path).expect("reopen db");
        assert_eq!(db.schema_version().expect("schema"), baseline_schema);
        assert_eq!(db.drawer_count().expect("drawers"), baseline_drawers);
        assert_eq!(db.triple_count().expect("triples"), baseline_triples);
        assert_eq!(db.taxonomy_count().expect("taxonomy"), baseline_taxonomy);
        assert_eq!(db.scope_counts().expect("scopes"), baseline_scopes);

        let search = run_search(&server, "debug", None, None, 1).await;
        assert_eq!(search.results[0].drawer_id, "drawer_shu");
        assert!(!search.results[0].content.is_empty());
    }

    #[test]
    fn test_mcp_tool_registry_includes_mempal_context() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();
        let search_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_search")
            .expect("mempal_search tool exists");
        let search_props = search_tool
            .input_schema
            .get("properties")
            .and_then(|value| value.as_object())
            .expect("mempal_search must have a properties object");
        assert!(
            search_props.get("scope").is_some(),
            "mempal_search must expose unified scope in tools/list"
        );

        let context_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_context")
            .expect("mempal_context tool exists");
        let context_props = context_tool
            .input_schema
            .get("properties")
            .and_then(|value| value.as_object())
            .expect("mempal_context must have a properties object");
        assert!(
            context_props.get("scope").is_some(),
            "mempal_context must expose unified scope in tools/list"
        );
        assert!(
            context_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("dao_tian -> dao_ren -> shu -> qi")
        );
    }

    #[test]
    fn test_mcp_tool_registry_includes_operation_status_and_ingest_wait_fields() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();

        let operation_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_operation_status")
            .expect("mempal_operation_status tool exists");
        assert!(
            operation_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("receipt-based write landed")
        );

        let ingest_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_ingest")
            .expect("mempal_ingest tool exists");
        let props = ingest_tool
            .input_schema
            .get("properties")
            .and_then(|value| value.as_object())
            .expect("mempal_ingest must have a properties object");

        for field in ["wait", "wait_timeout_secs"] {
            assert!(
                props.get(field).is_some(),
                "mempal_ingest must expose {field} in tools/list"
            );
        }
        for field in ["no_gate", "bypass_novelty"] {
            assert!(
                props.get(field).is_none(),
                "mempal_ingest must not expose {field} in tools/list"
            );
        }
    }

    #[tokio::test]
    async fn test_mcp_field_taxonomy_lists_stage1_fields() {
        let (_tempdir, _db_path, server) = setup_server();
        let response = server
            .field_taxonomy_json_for_test()
            .await
            .expect("field taxonomy should succeed");
        for field in [
            "general",
            "epistemics",
            "software-engineering",
            "tooling",
            "diary",
        ] {
            assert!(
                response.entries.iter().any(|entry| entry.field == field),
                "missing field {field}"
            );
        }
        let epistemics = response
            .entries
            .iter()
            .find(|entry| entry.field == "epistemics")
            .expect("epistemics field");
        assert!(epistemics.domains.iter().any(|domain| domain == "global"));
    }

    #[test]
    fn test_mcp_tool_registry_includes_mempal_field_taxonomy() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();
        let field_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_field_taxonomy")
            .expect("mempal_field_taxonomy tool exists");
        assert!(
            field_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("custom fields remain accepted")
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_policy_lists_stage1_thresholds() {
        let (_tempdir, _db_path, server) = setup_server();
        let response = server
            .knowledge_policy_json_for_test()
            .await
            .expect("policy should succeed");
        let dao_tian = response
            .entries
            .iter()
            .find(|entry| entry.tier == "dao_tian" && entry.target_status == "canonical")
            .expect("dao_tian policy");
        assert_eq!(dao_tian.requirements.min_supporting_refs, 3);
        assert_eq!(dao_tian.requirements.min_verification_refs, 2);
        assert_eq!(dao_tian.requirements.min_teaching_refs, 1);
        assert!(dao_tian.requirements.reviewer_required);

        let dao_ren = response
            .entries
            .iter()
            .find(|entry| entry.tier == "dao_ren" && entry.target_status == "promoted")
            .expect("dao_ren policy");
        assert_eq!(dao_ren.requirements.min_supporting_refs, 2);
        assert_eq!(dao_ren.requirements.min_verification_refs, 1);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_policy_has_no_db_side_effects() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "policy side-effect evidence",
            "mempal",
            Some("policy"),
            "/tmp/policy.md",
            2,
        );
        let db = Database::open(&db_path).expect("open db");
        let baseline_schema = db.schema_version().expect("schema");
        let baseline_drawers = db.drawer_count().expect("drawers");
        let baseline_triples = db.triple_count().expect("triples");
        let baseline_taxonomy = db.taxonomy_count().expect("taxonomy");

        for _ in 0..3 {
            let response = server
                .knowledge_policy_json_for_test()
                .await
                .expect("policy should succeed");
            assert!(!response.entries.is_empty());
        }

        let db = Database::open(&db_path).expect("reopen db");
        assert_eq!(db.schema_version().expect("schema"), baseline_schema);
        assert_eq!(db.drawer_count().expect("drawers"), baseline_drawers);
        assert_eq!(db.triple_count().expect("triples"), baseline_triples);
        assert_eq!(db.taxonomy_count().expect("taxonomy"), baseline_taxonomy);
    }

    #[test]
    fn test_mcp_tool_registry_includes_mempal_knowledge_policy() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();
        let policy_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_knowledge_policy")
            .expect("mempal_knowledge_policy tool exists");
        assert!(
            policy_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("Stage-1 knowledge promotion policy")
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_distill_creates_candidate_knowledge() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "evidence first observation",
            "mempal",
            Some("distill"),
            "/tmp/evidence.md",
            2,
        );

        let response = server
            .knowledge_distill_json_for_test(serde_json::json!({
                "statement": "Prefer evidence first",
                "content": "Use cited evidence before asserting project facts.",
                "tier": "dao_ren",
                "supporting_refs": ["drawer_evidence"]
            }))
            .await
            .expect("distill should succeed");
        assert!(response.created);
        assert!(!response.dry_run);
        assert!(response.drawer_id.starts_with("drawer_"));

        let db = Database::open(&db_path).expect("open db");
        let drawer = db
            .get_drawer(&response.drawer_id)
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(drawer.memory_kind, MemoryKind::Knowledge);
        assert_eq!(drawer.tier, Some(KnowledgeTier::DaoRen));
        assert_eq!(drawer.status, Some(KnowledgeStatus::Candidate));
        assert_eq!(drawer.supporting_refs, vec!["drawer_evidence"]);

        let context = server
            .context_json_for_test(serde_json::json!({
                "query": "evidence first",
                "cwd": db_path.parent().expect("db parent").to_string_lossy()
            }))
            .await
            .expect("context should succeed");
        let context_ids: Vec<_> = context
            .sections
            .into_iter()
            .flat_map(|section| section.items)
            .map(|item| item.drawer_id)
            .collect();
        assert!(!context_ids.contains(&response.drawer_id));
    }

    #[tokio::test]
    async fn test_mcp_knowledge_distill_dry_run_no_write() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "dry run evidence",
            "mempal",
            Some("distill"),
            "/tmp/evidence.md",
            2,
        );
        let db = Database::open(&db_path).expect("open db");
        let drawer_count_before = db.drawer_count().expect("drawer count");
        let vector_count_before = total_vector_count(&db);
        let schema_before = db.schema_version().expect("schema");
        let audit_before = audit_line_count(&db_path);

        let request = serde_json::json!({
            "statement": "Dry run candidate",
            "content": "This should not be written.",
            "tier": "qi",
            "supporting_refs": ["drawer_evidence"],
            "dry_run": true
        });
        let first = server
            .knowledge_distill_json_for_test(request.clone())
            .await
            .expect("first dry-run should succeed");
        let second = server
            .knowledge_distill_json_for_test(request)
            .await
            .expect("second dry-run should succeed");

        assert_eq!(first.drawer_id, second.drawer_id);
        assert!(!first.created);
        assert!(first.dry_run);
        assert!(!second.created);
        assert!(second.dry_run);
        assert_eq!(
            db.drawer_count().expect("drawer count"),
            drawer_count_before
        );
        assert_eq!(total_vector_count(&db), vector_count_before);
        assert_eq!(db.schema_version().expect("schema"), schema_before);
        assert_eq!(audit_line_count(&db_path), audit_before);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_distill_rejects_dao_tian_candidate() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "dao tian evidence",
            "mempal",
            Some("distill"),
            "/tmp/evidence.md",
            2,
        );

        let error = server
            .knowledge_distill_json_for_test(serde_json::json!({
                "statement": "Universal law",
                "content": "This should not be candidate dao_tian.",
                "tier": "dao_tian",
                "supporting_refs": ["drawer_evidence"]
            }))
            .await
            .expect_err("dao_tian candidate should be rejected");
        assert!(
            error
                .to_string()
                .contains("distill only allows candidate dao_ren or qi"),
            "error={error}"
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_distill_rejects_missing_supporting_refs() {
        let (_tempdir, db_path, server) = setup_server();
        let missing = server
            .knowledge_distill_json_for_test(serde_json::json!({
                "statement": "Missing refs",
                "content": "This should fail before writing.",
                "tier": "qi",
                "supporting_refs": []
            }))
            .await
            .expect_err("missing refs should be rejected");
        assert!(
            missing.to_string().contains("supporting_refs"),
            "error={missing}"
        );

        insert_drawer(
            &db_path,
            "drawer_evidence",
            "support evidence",
            "mempal",
            Some("distill"),
            "/tmp/evidence.md",
            2,
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_ref_knowledge",
            KnowledgeTier::Qi,
            KnowledgeStatus::Candidate,
            "Tool candidate.",
            "Knowledge ref content",
            KnowledgeRefs {
                supporting: vec!["drawer_evidence".to_string()],
                ..KnowledgeRefs::default()
            },
        );

        let wrong_kind = server
            .knowledge_distill_json_for_test(serde_json::json!({
                "statement": "Wrong ref kind",
                "content": "This should fail before writing.",
                "tier": "qi",
                "supporting_refs": ["drawer_ref_knowledge"]
            }))
            .await
            .expect_err("knowledge refs should be rejected");
        assert!(
            wrong_kind
                .to_string()
                .contains("supporting_refs must point to evidence drawers"),
            "error={wrong_kind}"
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_distill_stores_trigger_hints() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "trigger hint evidence",
            "mempal",
            Some("distill"),
            "/tmp/evidence.md",
            2,
        );

        let response = server
            .knowledge_distill_json_for_test(serde_json::json!({
                "statement": "Reproduce before patching",
                "content": "Reproduce failures before changing code.",
                "tier": "qi",
                "supporting_refs": ["drawer_evidence"],
                "trigger_hints": {
                    "intent_tags": ["debugging"],
                    "workflow_bias": ["reproduce-first"],
                    "tool_needs": ["cargo-test"]
                }
            }))
            .await
            .expect("distill should succeed");
        let db = Database::open(&db_path).expect("open db");
        let drawer = db
            .get_drawer(&response.drawer_id)
            .expect("load drawer")
            .expect("drawer exists");
        let hints = drawer.trigger_hints.expect("trigger hints");
        assert_eq!(hints.intent_tags, vec!["debugging"]);
        assert_eq!(hints.workflow_bias, vec!["reproduce-first"]);
        assert_eq!(hints.tool_needs, vec!["cargo-test"]);
        assert!(
            crate::core::protocol::MEMORY_PROTOCOL.contains("trigger_hints as bias metadata only")
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_distill_existing_drawer_no_duplicate_or_audit() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "idempotent evidence",
            "mempal",
            Some("distill"),
            "/tmp/evidence.md",
            2,
        );
        let request = serde_json::json!({
            "statement": "Idempotent distill",
            "content": "Equivalent requests should not duplicate drawers.",
            "tier": "dao_ren",
            "supporting_refs": ["drawer_evidence"]
        });
        let first = server
            .knowledge_distill_json_for_test(request.clone())
            .await
            .expect("first distill should create");
        assert!(first.created);
        let db = Database::open(&db_path).expect("open db");
        let drawer_count_before_second = db.drawer_count().expect("drawer count");
        let vector_count_before_second = total_vector_count(&db);
        let audit_before_second = audit_line_count(&db_path);

        let second = server
            .knowledge_distill_json_for_test(request)
            .await
            .expect("second distill should be idempotent");
        assert_eq!(second.drawer_id, first.drawer_id);
        assert!(!second.created);
        assert_eq!(
            db.drawer_count().expect("drawer count"),
            drawer_count_before_second
        );
        assert_eq!(total_vector_count(&db), vector_count_before_second);
        assert_eq!(audit_line_count(&db_path), audit_before_second);
        assert_eq!(vector_row_count(&db, &first.drawer_id), 1);
    }

    #[test]
    fn test_mcp_tool_registry_and_protocol_include_mempal_knowledge_distill() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();
        let distill_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_knowledge_distill")
            .expect("mempal_knowledge_distill tool exists");
        assert!(
            distill_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("candidate knowledge from existing evidence")
        );
        assert!(crate::core::protocol::MEMORY_PROTOCOL.contains("mempal_knowledge_distill"));
    }

    #[tokio::test]
    async fn test_mcp_knowledge_gate_allows_dao_ren_promotion() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_support_1",
            "support 1",
            "mempal",
            Some("gate"),
            "/tmp/support-1.md",
            2,
        );
        insert_drawer(
            &db_path,
            "drawer_support_2",
            "support 2",
            "mempal",
            Some("gate"),
            "/tmp/support-2.md",
            2,
        );
        insert_drawer(
            &db_path,
            "drawer_verify_1",
            "verify 1",
            "mempal",
            Some("gate"),
            "/tmp/verify-1.md",
            2,
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_knowledge_gate",
            KnowledgeTier::DaoRen,
            KnowledgeStatus::Candidate,
            "Domain rules need evidence.",
            "Knowledge content",
            KnowledgeRefs {
                supporting: vec![
                    "drawer_support_1".to_string(),
                    "drawer_support_2".to_string(),
                ],
                verification: vec!["drawer_verify_1".to_string()],
                ..KnowledgeRefs::default()
            },
        );

        let response = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_knowledge_gate"
            }))
            .await
            .expect("gate should succeed");

        assert!(response.allowed, "reasons={:?}", response.reasons);
        assert_eq!(response.target_status, "promoted");
        assert_eq!(response.evidence_counts.supporting, 2);
        assert_eq!(response.evidence_counts.verification, 1);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_gate_rejects_missing_verification() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_support_1",
            "support 1",
            "mempal",
            Some("gate"),
            "/tmp/support-1.md",
            2,
        );
        insert_drawer(
            &db_path,
            "drawer_support_2",
            "support 2",
            "mempal",
            Some("gate"),
            "/tmp/support-2.md",
            2,
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_knowledge_gate",
            KnowledgeTier::DaoRen,
            KnowledgeStatus::Candidate,
            "Domain rules need verification.",
            "Knowledge content",
            KnowledgeRefs {
                supporting: vec![
                    "drawer_support_1".to_string(),
                    "drawer_support_2".to_string(),
                ],
                ..KnowledgeRefs::default()
            },
        );

        let db = Database::open(&db_path).expect("open db");
        let schema_before = db.schema_version().expect("schema");
        let drawer_count_before = db.drawer_count().expect("drawer count");
        let triple_count_before = db.triple_count().expect("triple count");
        let audit_before = audit_line_count(&db_path);

        let response = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_knowledge_gate"
            }))
            .await
            .expect("gate should return advisory denial");

        assert!(!response.allowed);
        assert!(
            response
                .reasons
                .iter()
                .any(|reason| reason.contains("verification evidence refs below requirement")),
            "reasons={:?}",
            response.reasons
        );
        assert_eq!(db.schema_version().expect("schema"), schema_before);
        assert_eq!(
            db.drawer_count().expect("drawer count"),
            drawer_count_before
        );
        assert_eq!(
            db.triple_count().expect("triple count"),
            triple_count_before
        );
        assert_eq!(audit_line_count(&db_path), audit_before);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_gate_requires_reviewer_for_dao_tian() {
        let (_tempdir, db_path, server) = setup_server();
        for id in [
            "drawer_support_1",
            "drawer_support_2",
            "drawer_support_3",
            "drawer_verify_1",
            "drawer_verify_2",
            "drawer_teach_1",
        ] {
            insert_drawer(
                &db_path,
                id,
                id,
                "mempal",
                Some("gate"),
                &format!("/tmp/{id}.md"),
                2,
            );
        }
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_knowledge_gate",
            KnowledgeTier::DaoTian,
            KnowledgeStatus::Canonical,
            "Stable cross-domain principle.",
            "Knowledge content",
            KnowledgeRefs {
                supporting: vec![
                    "drawer_support_1".to_string(),
                    "drawer_support_2".to_string(),
                    "drawer_support_3".to_string(),
                ],
                verification: vec!["drawer_verify_1".to_string(), "drawer_verify_2".to_string()],
                teaching: vec!["drawer_teach_1".to_string()],
                ..KnowledgeRefs::default()
            },
        );

        let without_reviewer = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_knowledge_gate"
            }))
            .await
            .expect("gate should return advisory denial");
        assert!(!without_reviewer.allowed);
        assert!(
            without_reviewer
                .reasons
                .iter()
                .any(|reason| reason.contains("reviewer is required")),
            "reasons={:?}",
            without_reviewer.reasons
        );

        let with_reviewer = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_knowledge_gate",
                "reviewer": "alex"
            }))
            .await
            .expect("gate should allow with reviewer");
        assert!(with_reviewer.allowed, "reasons={:?}", with_reviewer.reasons);
        assert_eq!(with_reviewer.target_status, "canonical");
    }

    #[tokio::test]
    async fn test_mcp_knowledge_gate_blocks_counterexamples_by_default() {
        let (_tempdir, db_path, server) = setup_server();
        for id in ["drawer_support_1", "drawer_verify_1", "drawer_counter_1"] {
            insert_drawer(
                &db_path,
                id,
                id,
                "mempal",
                Some("gate"),
                &format!("/tmp/{id}.md"),
                2,
            );
        }
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_knowledge_gate",
            KnowledgeTier::Shu,
            KnowledgeStatus::Promoted,
            "Reusable method.",
            "Knowledge content",
            KnowledgeRefs {
                supporting: vec!["drawer_support_1".to_string()],
                verification: vec!["drawer_verify_1".to_string()],
                counterexample: vec!["drawer_counter_1".to_string()],
                ..KnowledgeRefs::default()
            },
        );

        let blocked = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_knowledge_gate"
            }))
            .await
            .expect("gate should return advisory denial");
        assert!(!blocked.allowed);
        assert!(
            blocked
                .reasons
                .iter()
                .any(|reason| reason.contains("counterexample refs present")),
            "reasons={:?}",
            blocked.reasons
        );

        let allowed = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_knowledge_gate",
                "allow_counterexamples": true
            }))
            .await
            .expect("gate should allow explicit counterexample override");
        assert!(allowed.allowed, "reasons={:?}", allowed.reasons);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_gate_rejects_evidence_drawer() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence",
            "evidence",
            "mempal",
            Some("gate"),
            "/tmp/evidence.md",
            2,
        );

        let error = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_evidence"
            }))
            .await
            .expect_err("evidence drawer should be rejected");
        assert!(
            error.to_string().contains("knowledge drawer"),
            "error={error}"
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_gate_validates_role_refs() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_support_1",
            "support",
            "mempal",
            Some("gate"),
            "/tmp/support.md",
            2,
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_ref_knowledge",
            KnowledgeTier::Qi,
            KnowledgeStatus::Candidate,
            "Tool capability.",
            "Knowledge ref content",
            KnowledgeRefs {
                supporting: vec!["drawer_support_1".to_string()],
                ..KnowledgeRefs::default()
            },
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_knowledge_gate",
            KnowledgeTier::DaoRen,
            KnowledgeStatus::Candidate,
            "Domain rule.",
            "Knowledge content",
            KnowledgeRefs {
                supporting: vec![
                    "drawer_support_1".to_string(),
                    "drawer_support_1".to_string(),
                ],
                verification: vec!["drawer_ref_knowledge".to_string()],
                ..KnowledgeRefs::default()
            },
        );

        let error = server
            .knowledge_gate_json_for_test(serde_json::json!({
                "drawer_id": "drawer_knowledge_gate"
            }))
            .await
            .expect_err("knowledge ref should be rejected");
        assert!(
            error
                .to_string()
                .contains("gate refs must point to evidence drawers"),
            "error={error}"
        );
    }

    #[test]
    fn test_mcp_tool_registry_and_protocol_include_mempal_knowledge_gate() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();
        let gate_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_knowledge_gate")
            .expect("mempal_knowledge_gate tool exists");
        assert!(
            gate_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("Read-only promotion readiness")
        );
        assert!(crate::core::protocol::MEMORY_PROTOCOL.contains("mempal_knowledge_gate"));
    }

    #[tokio::test]
    async fn test_mcp_knowledge_promote_updates_status_after_gate_pass() {
        let (_tempdir, db_path, server) = setup_server();
        for id in ["drawer_support_1", "drawer_support_2", "drawer_verify_1"] {
            insert_drawer(
                &db_path,
                id,
                id,
                "mempal",
                Some("lifecycle"),
                &format!("/tmp/{id}.md"),
                2,
            );
        }
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_lifecycle_promote",
            KnowledgeTier::DaoRen,
            KnowledgeStatus::Candidate,
            "Gate-passed knowledge can be promoted.",
            "Knowledge content",
            KnowledgeRefs {
                supporting: vec![
                    "drawer_support_1".to_string(),
                    "drawer_support_2".to_string(),
                ],
                ..KnowledgeRefs::default()
            },
        );
        let audit_before = audit_line_count(&db_path);

        let response = server
            .knowledge_promote_json_for_test(serde_json::json!({
                "drawer_id": "drawer_lifecycle_promote",
                "status": "promoted",
                "verification_refs": ["drawer_verify_1"],
                "reason": "validated by MCP lifecycle test",
                "reviewer": "test"
            }))
            .await
            .expect("promote should pass");

        assert_eq!(response.old_status, "candidate");
        assert_eq!(response.new_status, "promoted");
        let gate = response.gate.expect("MCP promote returns gate report");
        assert!(gate.allowed, "reasons={:?}", gate.reasons);
        assert_eq!(response.verification_refs, vec!["drawer_verify_1"]);
        let db = Database::open(&db_path).expect("open db");
        let drawer = db
            .get_drawer("drawer_lifecycle_promote")
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(drawer.status, Some(KnowledgeStatus::Promoted));
        assert_eq!(drawer.verification_refs, vec!["drawer_verify_1"]);
        assert_eq!(audit_line_count(&db_path), audit_before + 1);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_promote_rejects_gate_failure_without_mutation() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_support_1",
            "support 1",
            "mempal",
            Some("lifecycle"),
            "/tmp/support-1.md",
            2,
        );
        insert_drawer(
            &db_path,
            "drawer_verify_1",
            "verify 1",
            "mempal",
            Some("lifecycle"),
            "/tmp/verify-1.md",
            2,
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_lifecycle_gate_fail",
            KnowledgeTier::DaoRen,
            KnowledgeStatus::Candidate,
            "Insufficiently supported knowledge cannot be promoted.",
            "Knowledge content",
            KnowledgeRefs {
                supporting: vec!["drawer_support_1".to_string()],
                ..KnowledgeRefs::default()
            },
        );
        let db = Database::open(&db_path).expect("open db");
        let schema_before = db.schema_version().expect("schema");
        let vector_count_before = vector_row_count(&db, "drawer_lifecycle_gate_fail");
        let audit_before = audit_line_count(&db_path);

        let error = server
            .knowledge_promote_json_for_test(serde_json::json!({
                "drawer_id": "drawer_lifecycle_gate_fail",
                "status": "promoted",
                "verification_refs": ["drawer_verify_1"],
                "reason": "should fail gate"
            }))
            .await
            .expect_err("promote should fail gate");

        assert!(
            error.to_string().contains("promotion gate failed"),
            "error={error}"
        );
        let drawer = db
            .get_drawer("drawer_lifecycle_gate_fail")
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(drawer.status, Some(KnowledgeStatus::Candidate));
        assert!(drawer.verification_refs.is_empty());
        assert_eq!(db.schema_version().expect("schema"), schema_before);
        assert_eq!(
            vector_row_count(&db, "drawer_lifecycle_gate_fail"),
            vector_count_before
        );
        assert_eq!(audit_line_count(&db_path), audit_before);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_demote_updates_status_and_counterexample_refs() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_counterexample_1",
            "counterexample 1",
            "mempal",
            Some("lifecycle"),
            "/tmp/counterexample-1.md",
            2,
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_lifecycle_demote",
            KnowledgeTier::Shu,
            KnowledgeStatus::Promoted,
            "A workflow can be demoted.",
            "Knowledge content",
            KnowledgeRefs::default(),
        );
        let audit_before = audit_line_count(&db_path);

        let response = server
            .knowledge_demote_json_for_test(serde_json::json!({
                "drawer_id": "drawer_lifecycle_demote",
                "status": "demoted",
                "evidence_refs": ["drawer_counterexample_1"],
                "reason": "contradicted by MCP lifecycle test",
                "reason_type": "contradicted"
            }))
            .await
            .expect("demote should pass");

        assert_eq!(response.old_status, "promoted");
        assert_eq!(response.new_status, "demoted");
        assert_eq!(
            response.counterexample_refs,
            vec!["drawer_counterexample_1"]
        );
        let db = Database::open(&db_path).expect("open db");
        let drawer = db
            .get_drawer("drawer_lifecycle_demote")
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(drawer.status, Some(KnowledgeStatus::Demoted));
        assert_eq!(drawer.counterexample_refs, vec!["drawer_counterexample_1"]);
        assert_eq!(audit_line_count(&db_path), audit_before + 1);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_lifecycle_rejects_evidence_drawer_targets() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_evidence_target",
            "evidence target",
            "mempal",
            Some("lifecycle"),
            "/tmp/evidence-target.md",
            2,
        );
        let promote_error = server
            .knowledge_promote_json_for_test(serde_json::json!({
                "drawer_id": "drawer_evidence_target",
                "status": "promoted",
                "verification_refs": ["drawer_evidence_target"],
                "reason": "bad target"
            }))
            .await
            .expect_err("evidence target should be rejected");
        assert!(
            promote_error
                .to_string()
                .contains("knowledge lifecycle requires a knowledge drawer"),
            "error={promote_error}"
        );

        let demote_error = server
            .knowledge_demote_json_for_test(serde_json::json!({
                "drawer_id": "drawer_evidence_target",
                "status": "demoted",
                "evidence_refs": ["drawer_evidence_target"],
                "reason": "bad target",
                "reason_type": "contradicted"
            }))
            .await
            .expect_err("evidence target should be rejected");
        assert!(
            demote_error
                .to_string()
                .contains("knowledge lifecycle requires a knowledge drawer"),
            "error={demote_error}"
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_lifecycle_validates_refs_are_evidence_drawers() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_lifecycle_target",
            KnowledgeTier::Qi,
            KnowledgeStatus::Candidate,
            "Knowledge target.",
            "Knowledge content",
            KnowledgeRefs::default(),
        );
        insert_knowledge_drawer_with_refs(
            &db_path,
            "drawer_wrong_ref_kind",
            KnowledgeTier::Qi,
            KnowledgeStatus::Candidate,
            "Wrong ref kind.",
            "Knowledge content",
            KnowledgeRefs::default(),
        );

        let promote_error = server
            .knowledge_promote_json_for_test(serde_json::json!({
                "drawer_id": "drawer_lifecycle_target",
                "status": "promoted",
                "verification_refs": ["drawer_wrong_ref_kind"],
                "reason": "bad ref"
            }))
            .await
            .expect_err("knowledge ref should be rejected");
        assert!(
            promote_error
                .to_string()
                .contains("lifecycle refs must point to evidence drawers"),
            "error={promote_error}"
        );

        let demote_error = server
            .knowledge_demote_json_for_test(serde_json::json!({
                "drawer_id": "drawer_lifecycle_target",
                "status": "demoted",
                "evidence_refs": ["drawer_wrong_ref_kind"],
                "reason": "bad ref",
                "reason_type": "contradicted"
            }))
            .await
            .expect_err("knowledge ref should be rejected");
        assert!(
            demote_error
                .to_string()
                .contains("lifecycle refs must point to evidence drawers"),
            "error={demote_error}"
        );
    }

    #[test]
    fn test_mcp_tool_registry_and_protocol_include_knowledge_lifecycle_tools() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();
        let promote_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_knowledge_promote")
            .expect("mempal_knowledge_promote tool exists");
        assert!(
            promote_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("gate pass")
        );
        let demote_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_knowledge_demote")
            .expect("mempal_knowledge_demote tool exists");
        assert!(
            demote_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("counterexample evidence")
        );
        assert!(crate::core::protocol::MEMORY_PROTOCOL.contains("mempal_knowledge_promote"));
        assert!(crate::core::protocol::MEMORY_PROTOCOL.contains("MCP promotion is gate-enforced"));
    }

    #[tokio::test]
    async fn test_mcp_knowledge_publish_anchor_worktree_to_repo() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer_with_anchor(
            &db_path,
            "drawer_publish_worktree",
            KnowledgeStatus::Promoted,
            KnowledgeAnchorArgs {
                domain: MemoryDomain::Project,
                anchor_kind: AnchorKind::Worktree,
                anchor_id: "worktree:///tmp/mcp-publish-worktree",
                parent_anchor_id: Some("repo://parent"),
            },
        );
        let db = Database::open(&db_path).expect("open db");
        let before = db
            .get_drawer("drawer_publish_worktree")
            .expect("load drawer")
            .expect("drawer exists");
        let schema_before = db.schema_version().expect("schema");
        let vector_count_before = vector_row_count(&db, "drawer_publish_worktree");
        let audit_before = audit_line_count(&db_path);

        let response = server
            .knowledge_publish_anchor_json_for_test(serde_json::json!({
                "drawer_id": "drawer_publish_worktree",
                "to": "repo",
                "reason": "share stable MCP rule"
            }))
            .await
            .expect("publish should pass");

        assert_eq!(response.old_anchor_kind, "worktree");
        assert_eq!(
            response.old_anchor_id,
            "worktree:///tmp/mcp-publish-worktree"
        );
        assert_eq!(response.new_anchor_kind, "repo");
        assert_eq!(response.new_anchor_id, "repo://parent");
        assert_eq!(response.new_parent_anchor_id, None);
        let after = db
            .get_drawer("drawer_publish_worktree")
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(after.anchor_kind, AnchorKind::Repo);
        assert_eq!(after.anchor_id, "repo://parent");
        assert_eq!(after.parent_anchor_id, None);
        assert_eq!(after.content, before.content);
        assert_eq!(after.statement, before.statement);
        assert_eq!(after.status, before.status);
        assert_eq!(after.supporting_refs, before.supporting_refs);
        assert_eq!(db.schema_version().expect("schema"), schema_before);
        assert_eq!(
            vector_row_count(&db, "drawer_publish_worktree"),
            vector_count_before
        );
        assert_eq!(audit_line_count(&db_path), audit_before + 1);
        assert_eq!(
            last_audit_entry(&db_path)["command"],
            "knowledge_publish_anchor"
        );
    }

    #[tokio::test]
    async fn test_mcp_knowledge_publish_anchor_repo_to_global() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer_with_anchor(
            &db_path,
            "drawer_publish_global",
            KnowledgeStatus::Canonical,
            KnowledgeAnchorArgs {
                domain: MemoryDomain::Global,
                anchor_kind: AnchorKind::Repo,
                anchor_id: "repo://global-ready",
                parent_anchor_id: None,
            },
        );

        let response = server
            .knowledge_publish_anchor_json_for_test(serde_json::json!({
                "drawer_id": "drawer_publish_global",
                "to": "global",
                "target_anchor_id": "global://epistemics",
                "reason": "global law",
                "reviewer": "human"
            }))
            .await
            .expect("publish should pass");

        assert_eq!(response.new_anchor_kind, "global");
        assert_eq!(response.new_anchor_id, "global://epistemics");
        let db = Database::open(&db_path).expect("open db");
        let drawer = db
            .get_drawer("drawer_publish_global")
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(drawer.anchor_kind, AnchorKind::Global);
        assert_eq!(drawer.anchor_id, "global://epistemics");
        assert_eq!(last_audit_entry(&db_path)["details"]["reviewer"], "human");
    }

    #[tokio::test]
    async fn test_mcp_knowledge_publish_anchor_rejects_invalid_chain_without_mutation() {
        let (_tempdir, db_path, server) = setup_server();
        insert_knowledge_drawer_with_anchor(
            &db_path,
            "drawer_publish_invalid_chain",
            KnowledgeStatus::Promoted,
            KnowledgeAnchorArgs {
                domain: MemoryDomain::Global,
                anchor_kind: AnchorKind::Worktree,
                anchor_id: "worktree:///tmp/mcp-publish-invalid",
                parent_anchor_id: Some("repo://parent"),
            },
        );
        let db = Database::open(&db_path).expect("open db");
        let before = db
            .get_drawer("drawer_publish_invalid_chain")
            .expect("load drawer")
            .expect("drawer exists");
        let schema_before = db.schema_version().expect("schema");
        let vector_count_before = vector_row_count(&db, "drawer_publish_invalid_chain");
        let audit_before = audit_line_count(&db_path);

        let error = server
            .knowledge_publish_anchor_json_for_test(serde_json::json!({
                "drawer_id": "drawer_publish_invalid_chain",
                "to": "global",
                "target_anchor_id": "global://x",
                "reason": "skip chain"
            }))
            .await
            .expect_err("invalid chain should fail");

        assert!(
            error
                .to_string()
                .contains("worktree -> global publication is not allowed"),
            "error={error}"
        );
        let after = db
            .get_drawer("drawer_publish_invalid_chain")
            .expect("load drawer")
            .expect("drawer exists");
        assert_eq!(after.anchor_kind, before.anchor_kind);
        assert_eq!(after.anchor_id, before.anchor_id);
        assert_eq!(after.parent_anchor_id, before.parent_anchor_id);
        assert_eq!(db.schema_version().expect("schema"), schema_before);
        assert_eq!(
            vector_row_count(&db, "drawer_publish_invalid_chain"),
            vector_count_before
        );
        assert_eq!(audit_line_count(&db_path), audit_before);
    }

    #[tokio::test]
    async fn test_mcp_knowledge_publish_anchor_rejects_inactive_or_evidence() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "drawer_publish_evidence",
            "evidence",
            "mempal",
            Some("publish"),
            "/tmp/publish-evidence.md",
            2,
        );
        insert_knowledge_drawer_with_anchor(
            &db_path,
            "drawer_publish_candidate",
            KnowledgeStatus::Candidate,
            KnowledgeAnchorArgs {
                domain: MemoryDomain::Project,
                anchor_kind: AnchorKind::Worktree,
                anchor_id: "worktree:///tmp/mcp-publish-candidate",
                parent_anchor_id: Some("repo://parent"),
            },
        );

        let evidence_error = server
            .knowledge_publish_anchor_json_for_test(serde_json::json!({
                "drawer_id": "drawer_publish_evidence",
                "to": "repo",
                "reason": "bad"
            }))
            .await
            .expect_err("evidence should be rejected");
        assert!(
            evidence_error.to_string().contains("knowledge drawer"),
            "error={evidence_error}"
        );

        let candidate_error = server
            .knowledge_publish_anchor_json_for_test(serde_json::json!({
                "drawer_id": "drawer_publish_candidate",
                "to": "repo",
                "reason": "bad"
            }))
            .await
            .expect_err("candidate should be rejected");
        assert!(
            candidate_error
                .to_string()
                .contains("promoted or canonical"),
            "error={candidate_error}"
        );
    }

    #[test]
    fn test_mcp_tool_registry_and_protocol_include_knowledge_publish_anchor() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();
        let publish_tool = tools
            .iter()
            .find(|tool| tool.name == "mempal_knowledge_publish_anchor")
            .expect("mempal_knowledge_publish_anchor tool exists");
        assert!(
            publish_tool
                .description
                .as_deref()
                .unwrap_or_default()
                .contains("outward across anchor scope")
        );
        assert!(crate::core::protocol::MEMORY_PROTOCOL.contains("mempal_knowledge_publish_anchor"));
        assert!(
            crate::core::protocol::MEMORY_PROTOCOL
                .contains("Anchor publication is separate from tier/status promotion")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mcp_fact_check_round_trip() {
        let (_tempdir, db_path, server) = setup_server();
        insert_triple(
            &db_path,
            "Bob",
            "husband_of",
            "Alice",
            Some("1799900000"),
            None,
        );
        insert_triple(
            &db_path,
            "Alice",
            "works_at",
            "Acme",
            Some("1700000000"),
            Some("1799999999"),
        );

        let response = server
            .mempal_fact_check(Parameters(FactCheckRequest {
                text: "Bob is Alice's brother. Alice works at Acme.".to_string(),
                wing: None,
                room: None,
                now: Some("2027-01-15T08:00:00Z".to_string()),
            }))
            .await
            .expect("fact check should succeed")
            .0;

        assert_eq!(response.issues.len(), 2, "issues={:?}", response.issues);

        let json = serde_json::to_vec(&response).expect("serialize");
        let back: FactCheckResponse = serde_json::from_slice(&json).expect("deserialize");
        assert_eq!(back.issues, response.issues);
        assert_eq!(back.checked_entities, response.checked_entities);
        assert_eq!(back.kg_triples_scanned, response.kg_triples_scanned);
    }

    #[tokio::test]
    async fn test_mcp_fact_check_invalid_scope_maps_to_invalid_params() {
        let (_tempdir, _db_path, server) = setup_server();

        let err = match server
            .mempal_fact_check(Parameters(FactCheckRequest {
                text: "Bob is Alice's brother".to_string(),
                wing: None,
                room: Some("design".to_string()),
                now: None,
            }))
            .await
        {
            Ok(_) => panic!("room without wing must be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("room requires wing"),
            "expected invalid scope error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_mcp_fact_check_invalid_now_maps_to_invalid_params() {
        let (_tempdir, _db_path, server) = setup_server();

        let err = match server
            .mempal_fact_check(Parameters(FactCheckRequest {
                text: "Bob is Alice's brother".to_string(),
                wing: None,
                room: None,
                now: Some("not-a-timestamp".to_string()),
            }))
            .await
        {
            Ok(_) => panic!("invalid now must be rejected"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains("expected RFC3339"),
            "expected invalid now error, got: {err}"
        );
    }

    async fn ingest_manual(
        server: &MempalMcpServer,
        content: &str,
        project_id: Option<&str>,
        supersedes: Option<&str>,
        replace_text: Option<&str>,
    ) -> IngestResponse {
        let request = IngestRequest {
            content: content.to_string(),
            wing: "mempal".to_string(),
            room: Some("replace".to_string()),
            project_id: project_id.map(ToOwned::to_owned),
            supersedes: supersedes.map(ToOwned::to_owned),
            replace_text: replace_text.map(ToOwned::to_owned),
            ..IngestRequest::default()
        };
        server
            .ingest_json_for_test(serde_json::to_value(request).expect("serialize ingest request"))
            .await
            .expect("ingest should succeed")
    }

    #[tokio::test]
    async fn test_mcp_ingest_idempotent_exact_duplicate_returns_existing_id() {
        let (_tempdir, db_path, server) = setup_server();

        let first = ingest_manual(&server, "idempotent exact fact", None, None, None).await;
        let second = ingest_manual(&server, "idempotent exact fact", None, None, None).await;

        assert_eq!(second.drawer_id, first.drawer_id);
        assert_eq!(second.drawer_ids, vec![first.drawer_id.clone()]);
        let db = Database::open(&db_path).expect("open db");
        assert_eq!(db.drawer_count().expect("drawer count"), 1);
    }

    #[tokio::test]
    async fn test_mcp_ingest_warns_but_succeeds_when_vector_index_metric_is_stale() {
        let (_tempdir, db_path, server) = setup_server();
        recreate_vectors_with_metric(&db_path, "l2");

        let response = ingest_manual(
            &server,
            "stale metric ingest still succeeds",
            None,
            None,
            None,
        )
        .await;

        assert!(!response.drawer_id.is_empty());
        assert!(has_vector_index_warning(&response.system_warnings));
    }

    #[tokio::test]
    async fn test_mcp_ingest_valid_until_is_respected_by_search_include_expired() {
        let (_tempdir, _db_path, server) = setup_server();
        let ingested = server
            .ingest_json_for_test(
                serde_json::to_value(IngestRequest {
                    content: "temporal expired fact".to_string(),
                    wing: "mempal".to_string(),
                    room: Some("temporal".to_string()),
                    valid_until: Some("1".to_string()),
                    ..IngestRequest::default()
                })
                .expect("serialize ingest request"),
            )
            .await
            .expect("ingest should succeed");

        let hidden = run_search(
            &server,
            "temporal expired fact",
            Some("mempal"),
            Some("temporal"),
            10,
        )
        .await;
        assert!(
            hidden
                .results
                .iter()
                .all(|result| result.drawer_id != ingested.drawer_id),
            "expired drawer should be hidden by default"
        );

        let visible = server
            .mempal_search(Parameters(SearchRequest {
                query: "temporal expired fact".to_string(),
                wing: Some("mempal".to_string()),
                room: Some("temporal".to_string()),
                top_k: Some(10),
                include_expired: Some(true),
                ..SearchRequest::default()
            }))
            .await
            .expect("include expired search should succeed")
            .0;
        assert!(
            visible
                .results
                .iter()
                .any(|result| result.drawer_id == ingested.drawer_id),
            "include_expired should return the expired drawer"
        );
    }

    #[tokio::test]
    async fn test_mcp_ingest_supersedes_soft_deletes_old_and_links_new() {
        let (_tempdir, db_path, server) = setup_server();
        let old = ingest_manual(&server, "stale explicit fact", None, None, None).await;

        let new = ingest_manual(
            &server,
            "corrected explicit fact",
            None,
            Some(&old.drawer_id),
            None,
        )
        .await;

        assert_eq!(
            new.superseded_drawer_id.as_deref(),
            Some(old.drawer_id.as_str())
        );
        let db = Database::open(&db_path).expect("open db");
        assert!(db.get_drawer(&old.drawer_id).expect("old lookup").is_none());
        let new_drawer = db
            .get_drawer(&new.drawer_id)
            .expect("new lookup")
            .expect("new drawer exists");
        assert!(
            new_drawer
                .scope_constraints
                .as_deref()
                .unwrap_or_default()
                .contains(&format!("supersedes:{}", old.drawer_id))
        );
    }

    #[tokio::test]
    async fn test_mcp_ingest_supersedes_empty_chunks_errors_without_retiring_old() {
        let (_tempdir, db_path, server) = setup_server();
        let old = ingest_manual(&server, "empty replacement old fact", None, None, None).await;

        let response = server
            .ingest_json_for_test(
                serde_json::to_value(IngestRequest {
                    content: "   \n\t  ".to_string(),
                    wing: "mempal".to_string(),
                    room: Some("replace".to_string()),
                    supersedes: Some(old.drawer_id.clone()),
                    ..IngestRequest::default()
                })
                .expect("serialize ingest request"),
            )
            .await
            .expect("ingest should complete");

        assert_eq!(response.state, Some(IngestOperationState::Failed));
        assert!(
            response
                .failure_detail
                .as_deref()
                .unwrap_or_default()
                .contains("content produced no chunks")
        );
        let db = Database::open(&db_path).expect("open db");
        assert!(db.get_drawer(&old.drawer_id).expect("old lookup").is_some());
        assert_eq!(db.drawer_count().expect("drawer count"), 1);
    }

    #[tokio::test]
    async fn test_mcp_ingest_replace_text_single_match_supersedes() {
        let (_tempdir, db_path, server) = setup_server();
        let old = ingest_manual(&server, "replace text old fact", None, None, None).await;

        let new = ingest_manual(
            &server,
            "replace text new fact",
            None,
            None,
            Some("replace text old fact"),
        )
        .await;

        assert_eq!(
            new.superseded_drawer_id.as_deref(),
            Some(old.drawer_id.as_str())
        );
        let db = Database::open(&db_path).expect("open db");
        assert!(db.get_drawer(&old.drawer_id).expect("old lookup").is_none());
        assert!(db.get_drawer(&new.drawer_id).expect("new lookup").is_some());
    }

    #[tokio::test]
    async fn test_mcp_ingest_replace_text_zero_matches_errors() {
        let (_tempdir, _db_path, server) = setup_server();

        let error = match server
            .mempal_ingest(Parameters(IngestRequest {
                content: "new fact without old".to_string(),
                wing: "mempal".to_string(),
                room: Some("replace".to_string()),
                replace_text: Some("missing old fact".to_string()),
                ..IngestRequest::default()
            }))
            .await
        {
            Ok(_) => panic!("missing replace_text target should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("no matching active fact found"));
    }

    #[tokio::test]
    async fn test_mcp_ingest_replace_text_multiple_matches_errors_with_candidates() {
        let (_tempdir, db_path, server) = setup_server();
        insert_drawer(
            &db_path,
            "replace_candidate_a",
            "ambiguous old fact",
            "mempal",
            Some("replace"),
            "a.md",
            0,
        );
        insert_drawer(
            &db_path,
            "replace_candidate_b",
            "ambiguous old fact",
            "mempal",
            Some("replace"),
            "b.md",
            0,
        );

        let error = match server
            .mempal_ingest(Parameters(IngestRequest {
                content: "new ambiguous replacement".to_string(),
                wing: "mempal".to_string(),
                room: Some("replace".to_string()),
                replace_text: Some("ambiguous old fact".to_string()),
                ..IngestRequest::default()
            }))
            .await
        {
            Ok(_) => panic!("ambiguous replace_text target should fail"),
            Err(error) => error,
        };
        let message = error.to_string();

        assert!(message.contains("multiple matching active facts"));
        assert!(message.contains("replace_candidate_a"));
        assert!(message.contains("replace_candidate_b"));
    }

    #[tokio::test]
    async fn test_mcp_ingest_superseded_drawer_excluded_from_search() {
        let (_tempdir, _db_path, server) = setup_server();
        let old = ingest_manual(&server, "retired-search-token old fact", None, None, None).await;
        let new = ingest_manual(
            &server,
            "replacement visible fact",
            None,
            Some(&old.drawer_id),
            None,
        )
        .await;

        let results = server
            .mempal_search(Parameters(SearchRequest {
                query: "retired-search-token".to_string(),
                wing: Some("mempal".to_string()),
                room: Some("replace".to_string()),
                top_k: Some(10),
                ..SearchRequest::default()
            }))
            .await
            .expect("search should succeed")
            .0
            .results;

        assert!(
            !results
                .iter()
                .any(|result| result.drawer_id == old.drawer_id)
        );
        assert!(
            results
                .iter()
                .all(|result| result.drawer_id != old.drawer_id)
        );
        assert_ne!(new.drawer_id, old.drawer_id);
    }

    #[tokio::test]
    async fn test_mcp_ingest_cannot_supersede_different_project_scope() {
        let (_tempdir, _db_path, server) = setup_server();
        let old = ingest_manual(
            &server,
            "project scoped old fact",
            Some("project-a"),
            None,
            None,
        )
        .await;

        let error = match server
            .mempal_ingest(Parameters(IngestRequest {
                content: "project scoped new fact".to_string(),
                wing: "mempal".to_string(),
                room: Some("replace".to_string()),
                project_id: Some("project-b".to_string()),
                supersedes: Some(old.drawer_id),
                ..IngestRequest::default()
            }))
            .await
        {
            Ok(_) => panic!("project mismatch should fail"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("expected Some(\"project-b\")"));
    }

    #[tokio::test]
    async fn test_mcp_ingest_returns_receipt_before_embedder_runs() {
        let db_path = PathBuf::from("/tmp/mempal-async-receipt.db");
        Database::open(&db_path).expect("open db");

        let call_count = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(BlockingEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
                call_count: Arc::clone(&call_count),
                gate: Arc::clone(&gate),
            }),
        )
        .expect("create MCP server");

        let response = tokio::time::timeout(
            Duration::from_secs(2),
            server.mempal_ingest(Parameters(IngestRequest {
                content: "receipt path should be non-blocking".to_string(),
                wing: "mcp".to_string(),
                room: Some("receipt".to_string()),
                project_id: Some("project-async".to_string()),
                dry_run: Some(false),
                ..IngestRequest::default()
            })),
        )
        .await
        .expect("receipt timeout")
        .expect("receipt response")
        .0;
        assert_eq!(response.state, Some(IngestOperationState::Queued));
        let operation_id = response.operation_id.clone().expect("operation id");
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "embedder must not be called before the receipt is returned"
        );

        let db = server.open_db().expect("open db");
        let op_state: String = db
            .conn()
            .query_row(
                "SELECT op_state FROM pending_messages WHERE id = ?1",
                [&operation_id],
                |row| row.get(0),
            )
            .expect("read queued op_state");
        assert!(
            matches!(op_state.as_str(), "queued" | "running"),
            "receipt may return before or just after the drain worker claims the op, got {op_state}"
        );

        gate.notify_one();
        let completed = server
            .wait_for_operation_completion(&operation_id)
            .await
            .expect("ingest completion");
        assert_eq!(completed.state, Some(IngestOperationState::Completed));
        assert!(!completed.drawer_id.is_empty());
    }

    #[tokio::test]
    async fn test_mcp_ingest_wait_returns_terminal_result() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("open db");

        let call_count = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Notify::new());
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(BlockingEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
                call_count: Arc::clone(&call_count),
                gate: Arc::clone(&gate),
            }),
        )
        .expect("create MCP server");

        let request = IngestRequest {
            content: "wait path should return the final ingest result".to_string(),
            wing: "mcp".to_string(),
            room: Some("wait".to_string()),
            project_id: Some("project-async".to_string()),
            dry_run: Some(false),
            wait: Some(true),
            wait_timeout_secs: Some(30),
            ..IngestRequest::default()
        };
        let ingest = {
            let server = server.clone();
            tokio::spawn(async move {
                server
                    .mempal_ingest(Parameters(request))
                    .await
                    .expect("wait ingest should succeed")
                    .0
            })
        };

        tokio::time::timeout(Duration::from_secs(2), async {
            while call_count.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("embedder should start before timeout");
        gate.notify_one();

        let response = ingest.await.expect("join wait ingest");
        assert_eq!(response.state, Some(IngestOperationState::Completed));
        assert!(!response.drawer_id.is_empty());
        assert!(!response.timed_out);

        let operation_id = response.operation_id.as_deref().expect("operation id");
        let completed_status = server
            .operation_status_json_for_test(operation_id)
            .await
            .expect("completed status");
        assert!(
            completed_status.timings.contains_key("embedding_ms"),
            "completed status must include embedding_ms timing"
        );
        assert!(
            completed_status.timings.contains_key("db_write_ms"),
            "completed status must include db_write_ms timing"
        );
    }

    #[tokio::test]
    async fn test_mcp_ingest_wait_timeout_returns_receipt() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("open db");

        let gate = Arc::new(Notify::new());
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(BlockingEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
                call_count: Arc::new(AtomicUsize::new(0)),
                gate: Arc::clone(&gate),
            }),
        )
        .expect("create MCP server");

        let response = server
            .mempal_ingest(Parameters(IngestRequest {
                content: "timeout should return a receipt".to_string(),
                wing: "mcp".to_string(),
                room: Some("timeout".to_string()),
                project_id: Some("project-async".to_string()),
                dry_run: Some(false),
                wait: Some(true),
                wait_timeout_secs: Some(0),
                ..IngestRequest::default()
            }))
            .await
            .expect("timeout ingest response")
            .0;

        assert_eq!(response.state, Some(IngestOperationState::Queued));
        assert!(response.timed_out);
        let operation_id = response.operation_id.expect("operation id");

        gate.notify_one();
        let completed = server
            .wait_for_operation_completion(&operation_id)
            .await
            .expect("cleanup ingest completion");
        assert_eq!(completed.state, Some(IngestOperationState::Completed));
    }

    #[tokio::test]
    async fn test_mcp_async_ingest_queue_payload_is_scrubbed() {
        let config_home = tempfile::tempdir().expect("config tempdir");
        let config_db_path = config_home.path().join("palace.db");
        let _config_guard = ConfigOverrideGuard::install(&format!(
            r#"
db_path = "{}"

[privacy]
enabled = true
"#,
            config_db_path.display()
        ))
        .await;

        let (_tempdir, db_path, server) = setup_server();
        let raw_secret = "sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ1234567890ABCD";
        let raw_content = format!("queued content {raw_secret}");
        let raw_replace_text = format!("replace target {raw_secret}");
        let raw_source = format!("source {raw_secret}");
        let raw_source_file = format!("source-file {raw_secret}");
        let expected_content = ConfigHandle::scrub_content(&raw_content);
        let expected_replace_text = ConfigHandle::scrub_content(&raw_replace_text);
        let expected_source = ConfigHandle::scrub_content(&raw_source);
        let expected_source_file = ConfigHandle::scrub_content(&raw_source_file);

        insert_drawer(
            &db_path,
            "drawer_async_scrub_replace_target",
            &expected_replace_text,
            "mcp",
            Some("scrub"),
            "/tmp/mcp-async-scrub.md",
            1,
        );

        let request = IngestRequest {
            content: raw_content,
            wing: "mcp".to_string(),
            room: Some("scrub".to_string()),
            source: Some(raw_source),
            source_file: Some(raw_source_file),
            replace_text: Some(raw_replace_text),
            dry_run: Some(false),
            ..IngestRequest::default()
        };

        let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
        let project_id = server
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await
            .expect("resolve project");
        let prepared = server
            .prepare_async_ingest_operation(
                &request,
                IngestControls::default(),
                config.as_ref(),
                compiled_privacy.as_ref(),
                project_id,
            )
            .await
            .expect("prepare async ingest");
        let payload = serde_json::to_string(&prepared).expect("serialize prepared ingest");
        let decoded: PreparedIngestOperation =
            serde_json::from_str(&payload).expect("decode prepared ingest payload");

        assert_eq!(decoded.request.content, expected_content);
        assert_eq!(
            decoded.request.replace_text.as_deref(),
            Some(expected_replace_text.as_str())
        );
        assert_eq!(
            decoded.request.source.as_deref(),
            Some(expected_source.as_str())
        );
        assert_eq!(
            decoded.request.source_file.as_deref(),
            Some(expected_source_file.as_str())
        );
        assert_eq!(decoded.scrubbed_content, expected_content);
        assert_eq!(decoded.request.content, decoded.scrubbed_content);
        assert_eq!(decoded.controls, IngestControls::default());
        assert!(
            !payload.contains(raw_secret),
            "raw secret leaked into payload"
        );
    }

    #[tokio::test]
    async fn test_mcp_operation_status_tracks_reclaim_and_completion() {
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        Database::open(&db_path).expect("open db");

        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(StubEmbedderFactory {
                vector: vec![0.4, 0.5, 0.6],
            }),
        )
        .expect("create MCP server");
        let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
        let request = IngestRequest {
            content: "status tool async ingest".to_string(),
            wing: "mcp".to_string(),
            room: Some("status".to_string()),
            project_id: Some("project-async".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        };
        let project_id = server
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await
            .expect("resolve project");
        let prepared = server
            .prepare_async_ingest_operation(
                &request,
                IngestControls::default(),
                config.as_ref(),
                compiled_privacy.as_ref(),
                project_id,
            )
            .await
            .expect("prepare async ingest");
        let payload = serde_json::to_string(&prepared).expect("serialize prepared ingest");
        let queue = crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path);
        let operation_id = queue
            .enqueue(INGEST_ASYNC_KIND, &payload)
            .expect("enqueue async ingest");

        let queued = server
            .operation_status_json_for_test(&operation_id)
            .await
            .expect("queued status");
        assert_eq!(queued.state, Some(IngestOperationState::Queued));
        assert!(queued.drawer_id.is_empty());

        let claim = queue
            .claim_next_by_kind("worker-a", 60, INGEST_ASYNC_KIND)
            .expect("claim queued op")
            .expect("claimed queued op");
        let db = server.open_db().expect("open db");
        db.conn()
            .execute(
                "UPDATE pending_messages SET claimed_at = ?2, heartbeat_at = ?2 WHERE id = ?1",
                params![claim.id, 1_i64],
            )
            .expect("stale heartbeat");
        assert_eq!(queue.reclaim_stale(60).expect("reclaim stale"), 1);

        let reclaimed = server
            .operation_status_json_for_test(&operation_id)
            .await
            .expect("reclaimed status");
        assert_eq!(reclaimed.state, Some(IngestOperationState::Queued));

        let reclaimed_claim = queue
            .claim_next_by_kind("worker-b", 60, INGEST_ASYNC_KIND)
            .expect("reclaim claim")
            .expect("reclaimed queued op");
        let async_queue = AsyncPendingMessageStore::from_store(queue.clone());
        server
            .process_ingest_claim(&async_queue, "worker-b", reclaimed_claim)
            .await
            .expect("process reclaimed op");

        let completed = server
            .operation_status_json_for_test(&operation_id)
            .await
            .expect("completed status");
        assert_eq!(completed.state, Some(IngestOperationState::Completed));
        assert!(!completed.drawer_id.is_empty());

        let completed_record =
            crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path)
                .operation_status(&operation_id)
                .expect("load completed operation record")
                .expect("completed operation record exists");
        assert_eq!(
            completed_record.result_drawer_id.as_deref(),
            Some(completed.drawer_id.as_str())
        );
        assert_eq!(completed_record.rejected_reason.as_deref(), None);

        let final_status = server
            .operation_status_json_for_test(&operation_id)
            .await
            .expect("final status");
        assert_eq!(final_status.state, Some(IngestOperationState::Completed));
        assert_eq!(final_status.drawer_id, completed.drawer_id);
    }

    #[tokio::test]
    async fn test_mcp_operation_status_rejected_has_no_result_drawer_id() {
        let (_tempdir, db_path, server) = setup_server();
        let _config_guard = ConfigOverrideGuard::install(&format!(
            r#"
db_path = "{}"

[privacy]
enabled = false

[ingest_gating.fact_check]
enabled = true
reject_on_contradiction = true
"#,
            db_path.display()
        ))
        .await;
        insert_triple(
            &db_path,
            "Bob",
            "husband_of",
            "Alice",
            Some("1700000000"),
            None,
        );

        let response = server
            .ingest_json_for_test(
                serde_json::to_value(IngestRequest {
                    content: "Bob is Alice's brother.".to_string(),
                    wing: "mcp".to_string(),
                    room: Some("status".to_string()),
                    project_id: Some("project-async".to_string()),
                    wait: Some(true),
                    ..IngestRequest::default()
                })
                .expect("serialize ingest request"),
            )
            .await
            .expect("rejected ingest should complete");

        assert_eq!(response.state, Some(IngestOperationState::Rejected));
        assert!(response.drawer_id.is_empty());
        assert!(response.drawer_ids.is_empty());
        assert!(response.rejected_reason.is_some());

        let operation_id = response.operation_id.as_deref().expect("operation id");
        let rejected_record =
            crate::core::queue::PendingMessageStore::new_without_reclaim(&db_path)
                .operation_status(operation_id)
                .expect("load rejected operation record")
                .expect("rejected operation record exists");
        assert_eq!(rejected_record.result_drawer_id, None);
        assert!(rejected_record.rejected_reason.is_some());
    }

    #[tokio::test]
    async fn test_mcp_ingest_response_exposes_lock_wait() {
        let (_tempdir, _db_path, server) = setup_server();

        let response = server
            .ingest_json_for_test(
                serde_json::to_value(IngestRequest {
                    content: "same content for lock contention".to_string(),
                    wing: "mempal".to_string(),
                    room: Some("review".to_string()),
                    source: None,
                    source_file: None,
                    source_type: None,
                    confidence: None,
                    importance: None,
                    dry_run: None,
                    diary_rollup: None,
                    supersedes: None,
                    replace_text: None,
                    valid_from: None,
                    valid_until: None,
                    memory_kind: None,
                    domain: None,
                    field: None,
                    is_pinned: None,
                    provenance: None,
                    statement: None,
                    tier: None,
                    status: None,
                    supporting_refs: None,
                    counterexample_refs: None,
                    teaching_refs: None,
                    verification_refs: None,
                    scope_constraints: None,
                    trigger_hints: None,
                    anchor_kind: None,
                    anchor_id: None,
                    parent_anchor_id: None,
                    cwd: None,
                    project_id: None,
                    wait: None,
                    wait_timeout_secs: None,
                })
                .expect("serialize ingest request"),
            )
            .await
            .expect("ingest should succeed");

        assert!(
            response.lock_wait_ms.is_some(),
            "non-dry-run MCP ingest must expose lock_wait_ms"
        );

        let json = serde_json::to_value(&response).expect("serialize");
        assert!(
            json.get("lock_wait_ms").is_some(),
            "JSON must expose lock_wait_ms"
        );
    }

    #[tokio::test]
    async fn test_mcp_ingest_public_handler_forces_internal_controls_off() {
        let _tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = _tempdir.path().join("palace.db");
        Database::open(&db_path).expect("open db");
        let _config_guard = ConfigOverrideGuard::install(&format!(
            r#"
db_path = "{}"

[privacy]
enabled = false

[ingest_gating]
enabled = false
"#,
            db_path.display()
        ))
        .await;
        let gate = Arc::new(Notify::new());
        let call_count = Arc::new(AtomicUsize::new(0));
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(BlockingEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
                call_count: Arc::clone(&call_count),
                gate: Arc::clone(&gate),
            }),
        )
        .expect("create MCP server");

        let response = server
            .mempal_ingest(Parameters(IngestRequest {
                content: "public handler control test".to_string(),
                wing: "mempal".to_string(),
                room: Some("review".to_string()),
                dry_run: Some(false),
                ..IngestRequest::default()
            }))
            .await
            .expect("ingest should queue")
            .0;

        assert_eq!(response.state, Some(IngestOperationState::Queued));
        let operation_id = response.operation_id.as_deref().expect("operation id");
        tokio::time::timeout(Duration::from_secs(2), async {
            while call_count.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker should reach embed");
        let db = Database::open(&db_path).expect("open db");
        let payload: String = db
            .conn()
            .query_row(
                "SELECT payload FROM pending_messages WHERE id = ?1",
                [operation_id],
                |row| row.get(0),
            )
            .expect("read queued ingest payload");
        let decoded: PreparedIngestOperation =
            serde_json::from_str(&payload).expect("decode queued ingest payload");

        assert_eq!(decoded.controls, IngestControls::default());
        assert!(!decoded.controls.no_gate);
        assert!(!decoded.controls.bypass_novelty);

        gate.notify_one();

        let completed = server
            .wait_for_operation_completion(operation_id)
            .await
            .expect("cleanup ingest completion");
        assert_eq!(completed.state, Some(IngestOperationState::Completed));
    }

    // =========================================================================
    // mempal_cowork_push MCP handler tests (P8 task 7, Codex review round-2 #2)
    // =========================================================================
    //
    // These tests exercise the HANDLER itself — caller identity inference,
    // target auto-inference, self-push rejection, and InboxError → ErrorData
    // mapping. They complement the integration tests in tests/cowork_inbox.rs,
    // which only cover the CLI and inbox layers.

    use super::super::tools::CoworkPushRequest;
    use tokio::sync::Mutex as TokioMutex;

    // Tests below mutate $HOME env var to point mempal_home() at a tempdir.
    // Rust's default test runner runs tests in parallel threads, so they
    // would race on shared process state. Serialize them behind a process-
    // wide async Mutex whose guard CAN be held across .await points
    // (unlike std::sync::Mutex, which clippy rejects with await_holding_lock).
    // Every cowork push handler test must acquire this guard before
    // mutating $HOME and hold it for its entire lifetime.
    static COWORK_HOME_LOCK: TokioMutex<()> = TokioMutex::const_new(());

    async fn setup_cowork_home(
        tempdir: &TempDir,
    ) -> (PathBuf, PathBuf, tokio::sync::MutexGuard<'static, ()>) {
        // Lock FIRST before touching $HOME so no other parallel cowork
        // test can observe a half-written env var.
        let guard = COWORK_HOME_LOCK.lock().await;
        let home = tempdir.path().to_path_buf();
        let mempal_home = home.join(".mempal");
        let repo = home.join("proj");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        unsafe {
            std::env::set_var("HOME", &home);
        }
        (mempal_home, repo, guard)
    }

    #[tokio::test]
    async fn test_mcp_push_without_client_info_rejects_auto_target() {
        let (tempdir, _db_path, server) = setup_server();
        let (_mempal_home, repo, _guard) = setup_cowork_home(&tempdir).await;

        // client_name is None because we never called initialize().
        // Pushing without an explicit target must fail with "cannot infer".
        let result = server
            .mempal_cowork_push(Parameters(CoworkPushRequest {
                content: "hello".into(),
                target_tool: None,
                cwd: repo.to_string_lossy().into_owned(),
            }))
            .await;

        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("expected push to fail when client_name is None"),
        };
        // MCP error message must mention inference failure.
        assert!(
            err.to_string().contains("cannot infer"),
            "expected inference error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_mcp_push_succeeds_with_captured_client_name_and_auto_target() {
        let (tempdir, _db_path, server) = setup_server();
        let (mempal_home, repo, _guard) = setup_cowork_home(&tempdir).await;

        // Simulate a completed `initialize` handshake: caller identified
        // as "claude-code" (Claude Code's standard MCP client name).
        *server.client_name.lock().unwrap() = Some("claude-code".to_string());

        let response = match server
            .mempal_cowork_push(Parameters(CoworkPushRequest {
                content: "from claude to partner".into(),
                target_tool: None,
                cwd: repo.to_string_lossy().into_owned(),
            }))
            .await
        {
            Ok(r) => r,
            Err(e) => panic!("push should succeed with valid client_name: {e}"),
        };

        // Target auto-inferred as partner of Claude → Codex.
        assert_eq!(response.0.target_tool, "codex");
        assert!(response.0.inbox_size_after > 0);

        // Verify the message actually landed in the codex inbox by draining.
        let messages = crate::cowork::inbox::drain(&mempal_home, Tool::Codex, &repo).unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "from claude to partner");
        assert_eq!(messages[0].from, "claude");
    }

    #[tokio::test]
    async fn test_mcp_push_self_push_rejected_via_inbox_error_mapping() {
        let (tempdir, _db_path, server) = setup_server();
        let (_mempal_home, repo, _guard) = setup_cowork_home(&tempdir).await;

        // Caller is Codex, target explicitly Codex → SelfPush error from
        // inbox::push. Handler must map it to InvalidParams MCP error.
        *server.client_name.lock().unwrap() = Some("codex".to_string());

        let err = match server
            .mempal_cowork_push(Parameters(CoworkPushRequest {
                content: "would be self push".into(),
                target_tool: Some("codex".to_string()),
                cwd: repo.to_string_lossy().into_owned(),
            }))
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("expected self-push to be rejected"),
        };

        assert!(
            err.to_string().contains("self"),
            "expected self-push error, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_mcp_push_explicit_target_overrides_auto_inference() {
        let (tempdir, _db_path, server) = setup_server();
        let (mempal_home, repo, _guard) = setup_cowork_home(&tempdir).await;

        *server.client_name.lock().unwrap() = Some("claude-code".to_string());

        // Caller=Claude; auto would infer Codex. Override explicitly to Codex
        // (same effective target, but proves the explicit branch runs).
        let response = match server
            .mempal_cowork_push(Parameters(CoworkPushRequest {
                content: "explicit target".into(),
                target_tool: Some("codex".to_string()),
                cwd: repo.to_string_lossy().into_owned(),
            }))
            .await
        {
            Ok(r) => r,
            Err(e) => panic!("explicit target push should succeed: {e}"),
        };
        assert_eq!(response.0.target_tool, "codex");

        let messages = crate::cowork::inbox::drain(&mempal_home, Tool::Codex, &repo).unwrap();
        assert_eq!(messages.len(), 1);
    }

    #[tokio::test]
    async fn test_mcp_push_rejects_explicit_auto_target() {
        // Guard for Codex review finding 1: `target_tool="auto"` must NOT
        // be accepted as an explicit target. Per spec lines 37/39 target is
        // limited to claude|codex. Previously `Tool::from_str_ci` let "auto"
        // through, which would silently write to an orphan
        // ~/.mempal/cowork-inbox/auto/*.jsonl that no partner drains.
        let (tempdir, _db_path, server) = setup_server();
        let (mempal_home, repo, _guard) = setup_cowork_home(&tempdir).await;

        *server.client_name.lock().unwrap() = Some("claude-code".to_string());

        for bad in ["auto", "AUTO", "Auto"] {
            let err = match server
                .mempal_cowork_push(Parameters(CoworkPushRequest {
                    content: "should not land".into(),
                    target_tool: Some(bad.to_string()),
                    cwd: repo.to_string_lossy().into_owned(),
                }))
                .await
            {
                Err(e) => e,
                Ok(_) => panic!("target_tool={bad:?} must be rejected"),
            };
            assert!(
                err.to_string().contains("expected claude|codex"),
                "error for target_tool={bad:?} should mention expected targets, got: {err}"
            );
        }

        // And ensure nothing was written to the orphan `auto/` inbox dir.
        let auto_inbox_dir = mempal_home.join("cowork-inbox").join("auto");
        assert!(
            !auto_inbox_dir.exists(),
            "rejected push must not create orphan auto/ inbox dir at {}",
            auto_inbox_dir.display()
        );
    }

    /// Regression guard: every property schema emitted in `tools/list` must be
    /// a JSON object, never a bare boolean.  Claude Code's MCP client rejects
    /// the *entire* tool list when any property schema is a boolean `true`, so
    /// a single bad field silently drops all 30+ tools.
    ///
    /// Specifically guards `mempal_phase3.metadata` and `.report`, which were
    /// emitting `true` because schemars 1.x generates boolean schemas for
    /// `serde_json::Value` fields.
    #[test]
    fn test_no_boolean_property_schemas_in_tools_list() {
        let (_tempdir, _db_path, server) = setup_server();
        let tools = server.tool_router.list_all();

        let mut violations: Vec<String> = Vec::new();

        for tool in &tools {
            if let Some(serde_json::Value::Object(props)) = tool.input_schema.get("properties") {
                for (prop_name, schema) in props {
                    if schema.is_boolean() {
                        violations.push(format!("{}.{}", tool.name, prop_name));
                    }
                }
            }
        }

        assert!(
            violations.is_empty(),
            "tools/list contains boolean property schemas (Claude Code rejects these):\n  {}",
            violations.join("\n  ")
        );

        // Specific regression: mempal_phase3.metadata and .report must be objects.
        let phase3 = tools
            .iter()
            .find(|t| t.name == "mempal_phase3")
            .expect("mempal_phase3 tool must be registered");
        let props = phase3
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("mempal_phase3 must have a properties object");

        assert!(
            props
                .get("metadata")
                .map(|v| v.is_object())
                .unwrap_or(false),
            "mempal_phase3.metadata property schema must be a JSON object, got: {:?}",
            props.get("metadata")
        );
        assert!(
            props.get("report").map(|v| v.is_object()).unwrap_or(false),
            "mempal_phase3.report property schema must be a JSON object, got: {:?}",
            props.get("report")
        );
    }
}
