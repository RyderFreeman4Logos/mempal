use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::core::{
    config::ConfigHandle,
    db::Database,
    project::{ProjectSearchScope, infer_project_id_from_root_uri, validate_project_id},
    types::{Drawer, SourceType, Triple},
    utils::{build_triple_id, current_timestamp, source_file_or_synthetic},
};
use crate::cowork::{PeekError, PeekRequest as CoworkPeekRequest, Tool, peek_partner};
use crate::embed::{EmbedderFactory, global_embed_status};
use crate::ingest::gating::{
    GatingDecision, GatingRuntime, IngestCandidate, evaluate_tier1, evaluate_tier2,
};
use crate::ingest::novelty::{NoveltyAction, NoveltyCandidate, evaluate as evaluate_novelty};
use crate::search::{resolve_route, search_bm25_only, search_with_vector};
use anyhow::Context;
use rmcp::{
    ErrorData, Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    service::Peer,
    tool, tool_handler, tool_router,
};

use super::timeline::{TimelineRequest, TimelineResponse};
use super::tools::{
    ChunkerStatsDto, CoworkPushRequest, CoworkPushResponse, DeleteRequest, DeleteResponse,
    DuplicateWarning, EmbedStatusDto, FactCheckRequest, FactCheckResponse, IngestRequest,
    IngestResponse, KgRequest, KgResponse, KgStatsDto, MAX_READ_DRAWERS_MAX_COUNT,
    MAX_READ_DRAWERS_REQUEST_IDS, PeekMessageDto, PeekPartnerRequest, PeekPartnerResponse,
    QueueStatsDto, ReadDrawerRequest, ReadDrawerResponse, ReadDrawersRequest, ReadDrawersResponse,
    ScopeCount, ScrubStatsDto, SearchRequest, SearchResponse, SearchResultDto, StatusResponse,
    SystemWarning, TaxonomyEntryDto, TaxonomyRequest, TaxonomyResponse, TripleDto, TunnelDto,
    TunnelsResponse,
};

#[derive(Clone)]
pub struct MempalMcpServer {
    db_path: PathBuf,
    gating_runtime: Arc<GatingRuntime>,
    embedder_factory: Arc<dyn EmbedderFactory>,
    tool_router: ToolRouter<Self>,
    /// Captured via `initialize` override so `auto` peek mode can infer the
    /// partner from the calling MCP client's self-reported name.
    client_name: Arc<Mutex<Option<String>>>,
    client_project_id: Arc<Mutex<Option<String>>>,
    client_peer: Arc<Mutex<Option<Peer<rmcp::RoleServer>>>>,
}

impl MempalMcpServer {
    pub fn new(db_path: PathBuf, config: crate::core::config::Config) -> Self {
        Self::new_with_factory_and_config(
            db_path,
            config.clone(),
            Arc::new(crate::embed::ConfiguredEmbedderFactory::new(config)),
        )
    }

    pub fn new_with_factory(db_path: PathBuf, embedder_factory: Arc<dyn EmbedderFactory>) -> Self {
        Self::new_with_factory_and_config(
            db_path,
            ConfigHandle::current().as_ref().clone(),
            embedder_factory,
        )
    }

    pub fn new_with_factory_and_config(
        db_path: PathBuf,
        config: crate::core::config::Config,
        embedder_factory: Arc<dyn EmbedderFactory>,
    ) -> Self {
        Self {
            db_path,
            gating_runtime: Arc::new(GatingRuntime::new(config, Arc::clone(&embedder_factory))),
            embedder_factory,
            tool_router: Self::tool_router(),
            client_name: Arc::new(Mutex::new(None)),
            client_project_id: Arc::new(Mutex::new(None)),
            client_peer: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn serve_stdio(
        self,
    ) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleServer, Self>> {
        self.gating_runtime
            .initialize_from_config()
            .await
            .context("failed to initialize ingest gating")?;
        self.serve(rmcp::transport::stdio())
            .await
            .context("failed to initialize MCP stdio transport")
    }

    pub(super) fn open_db(&self) -> std::result::Result<Database, ErrorData> {
        Database::open(&self.db_path).map_err(|error| {
            ErrorData::internal_error(format!("failed to open database: {error}"), None)
        })
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
        if let Some(peer) = peer
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
        inserted_drawer_ids: &mut Vec<String>,
    ) -> std::result::Result<(), ErrorData> {
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
                inserted_drawer_ids.push(chunk_did.clone());
                continue;
            }
            let source_file = source_file_or_synthetic(chunk_did, request.source.as_deref());
            db.insert_drawer_with_project(
                &Drawer {
                    id: chunk_did.clone(),
                    content: chunk.clone(),
                    wing: request.wing.clone(),
                    room: request.room.clone(),
                    source_file: Some(source_file),
                    source_type: SourceType::Manual,
                    added_at: current_timestamp(),
                    chunk_index: Some(*chunk_idx as i64),
                    importance: request.importance.unwrap_or(0),
                },
                project_id,
            )
            .map_err(db_error)?;
            db.insert_vector_with_project(chunk_did, vector, project_id)
                .map_err(db_error)?;
            inserted_drawer_ids.push(chunk_did.clone());
        }
        Ok(())
    }
}

#[tool_router(router = tool_router)]
impl MempalMcpServer {
    #[tool(
        name = "mempal_status",
        description = "Return schema version, drawer counts, taxonomy counts, database size, scope breakdown, the AAAK format spec, and the memory protocol. Call once at session start if you haven't seen the protocol yet."
    )]
    pub async fn mempal_status(&self) -> std::result::Result<Json<StatusResponse>, ErrorData> {
        let cfg_meta = ConfigHandle::snapshot_meta();
        let config = ConfigHandle::current();
        let db = self.open_db()?;
        let queue_stats = crate::core::queue::PendingMessageStore::new(db.path())
            .map_err(|error| {
                ErrorData::internal_error(format!("queue init failed: {error}"), None)
            })?
            .stats()
            .map_err(|error| {
                ErrorData::internal_error(format!("queue stats failed: {error}"), None)
            })?;
        let embed_snapshot = global_embed_status().snapshot();
        let schema_version = db.schema_version().map_err(db_error)?;
        let drawer_count = db.drawer_count().map_err(db_error)?;
        let null_project_backfill_pending =
            db.null_project_backfill_pending_count().map_err(db_error)?;
        let taxonomy_count = db.taxonomy_count().map_err(db_error)?;
        let db_size_bytes = db.database_size_bytes().map_err(db_error)?;
        let scopes = db
            .scope_counts()
            .map_err(db_error)?
            .into_iter()
            .map(|(wing, room, drawer_count)| ScopeCount {
                wing,
                room,
                drawer_count,
            })
            .collect();
        let mut system_warnings = current_system_warnings();
        if null_project_backfill_pending > 0 {
            system_warnings.push(SystemWarning {
                level: "warn".to_string(),
                message: format!(
                    "{null_project_backfill_pending} drawers still have NULL project_id; run `mempal project migrate --project <id>` to backfill historical records"
                ),
                source: "project_isolation".to_string(),
            });
        }

        Ok(Json(StatusResponse {
            schema_version,
            drawer_count,
            taxonomy_count,
            db_size_bytes,
            config_version: cfg_meta.version,
            config_loaded_at_unix_ms: cfg_meta.loaded_at_unix_ms,
            scopes,
            aaak_spec: crate::aaak::generate_spec(),
            memory_protocol: crate::core::protocol::MEMORY_PROTOCOL.to_string(),
            embed_status: EmbedStatusDto {
                backend: config.embed.backend.clone(),
                base_url: config
                    .embed
                    .resolved_openai_base_url()
                    .map(ToOwned::to_owned),
                pending_count: queue_stats.pending,
                claimed_count: queue_stats.claimed,
                failed_count: queue_stats.failed,
                degraded: embed_snapshot.degraded,
                fail_count: embed_snapshot.fail_count,
                failure_count: embed_snapshot.fail_count,
                last_error: embed_snapshot.last_error,
                last_success_at_unix_ms: embed_snapshot.last_success_at_unix_ms,
            },
            queue_stats: QueueStatsDto {
                pending: queue_stats.pending,
                claimed: queue_stats.claimed,
                failed: queue_stats.failed,
                oldest_pending_age_secs: queue_stats.oldest_pending_age_secs,
            },
            scrub_stats: ScrubStatsDto::from(ConfigHandle::scrub_stats()),
            chunker_stats: ChunkerStatsDto::from(
                crate::ingest::chunk::global_chunker_stats().snapshot(),
            ),
            system_warnings,
        }))
    }

    #[tool(
        name = "mempal_search",
        description = "Search persistent project memory via vector embedding with optional wing/room filters. PREFER THIS over grepping files or guessing from general knowledge when answering ANY project-specific question — past decisions, design rationale, implementation details, bug history, how a component works, why something was built a certain way, or any other project knowledge. Every result includes drawer_id and source_file for citation, plus structured AAAK-derived signals (`entities`, `topics`, `flags`, `emotions`, `importance_stars`) for filtering and ranking."
    )]
    pub async fn mempal_search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> std::result::Result<Json<SearchResponse>, ErrorData> {
        let config = ConfigHandle::current();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let unresolved_scope = project_id.is_none() && !request.all_projects.unwrap_or(false);
        let scope = ProjectSearchScope::from_request(
            project_id,
            request.include_global.unwrap_or(false),
            request.all_projects.unwrap_or(false),
            config.search.strict_project_isolation,
        );
        let db = self.open_db()?;
        let route = resolve_route(
            &db,
            &request.query,
            request.wing.as_deref(),
            request.room.as_deref(),
        )
        .map_err(|error| ErrorData::internal_error(format!("routing failed: {error}"), None))?;
        let top_k = request.top_k.unwrap_or(10);
        let mut extra_warnings = Vec::new();
        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let results = match tokio::time::timeout(
            Duration::from_secs(config.embed.retry.search_deadline_secs),
            embedder.embed(&[request.query.as_str()]),
        )
        .await
        {
            Ok(Ok(vectors)) => {
                let query_vector = vectors.into_iter().next().ok_or_else(|| {
                    ErrorData::internal_error("embedder returned no query vector", None)
                })?;
                search_with_vector(
                    &db,
                    &request.query,
                    &query_vector,
                    route.clone(),
                    &scope,
                    top_k,
                )
                .map_err(|error| {
                    ErrorData::internal_error(format!("search failed: {error}"), None)
                })?
            }
            Ok(Err(error)) => {
                extra_warnings.push(SystemWarning {
                    level: "warn".to_string(),
                    message: "vector unavailable, BM25 fallback".to_string(),
                    source: "embed".to_string(),
                });
                search_bm25_only(&db, &request.query, route, &scope, top_k).map_err(|bm25_error| {
                    ErrorData::internal_error(
                        format!(
                            "search failed after vector fallback: {error}; bm25 fallback failed: {bm25_error}"
                        ),
                        None,
                    )
                })?
            }
            Err(_) => {
                extra_warnings.push(SystemWarning {
                    level: "warn".to_string(),
                    message: "vector unavailable, BM25 fallback".to_string(),
                    source: "embed".to_string(),
                });
                search_bm25_only(&db, &request.query, route, &scope, top_k).map_err(|error| {
                    ErrorData::internal_error(
                        format!("search deadline fallback failed: {error}"),
                        None,
                    )
                })?
            }
        };

        let mut system_warnings = current_system_warnings();
        system_warnings.extend(extra_warnings);
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
        name = "mempal_ingest",
        description = "Persist a decision, bug fix, or design insight to project memory. Call this when a decision is reached in conversation — include the rationale, not just the outcome. Wing is required; let mempal auto-route the room. Set dry_run=true to preview the drawer_id without writing."
    )]
    pub async fn mempal_ingest(
        &self,
        Parameters(request): Parameters<IngestRequest>,
    ) -> std::result::Result<Json<IngestResponse>, ErrorData> {
        let (config, compiled_privacy) = ConfigHandle::current_privacy_snapshot();
        let project_id = self
            .resolve_mcp_project_id(request.project_id.as_deref(), config.as_ref())
            .await?;
        let scrubbed_content =
            config.scrub_content_with_compiled(&request.content, compiled_privacy.as_ref());
        let room = request.room.as_deref();
        let db = self.open_db()?;
        let dry_run = request.dry_run.unwrap_or(false);

        // Check degraded embed status BEFORE building the embedder so that
        // non-dry-run writes fail fast with "writes are paused" (issue #57
        // regression guard).
        if !dry_run && global_embed_status().should_block_writes() {
            return Err(degraded_write_error());
        }

        // --- Chunk the scrubbed content (issue #57) ---
        // Build embedder early so the chunker can respect max_input_tokens.
        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let chunks =
            crate::ingest::prepare_chunks(&scrubbed_content, &config.chunker, embedder.as_ref());

        // Resolve drawer_id per chunk. The first chunk's drawer_id is the
        // primary identifier for backward compatibility.
        let mut chunk_drawer_ids: Vec<(usize, String, bool)> = Vec::with_capacity(chunks.len());
        for (idx, chunk) in chunks.iter().enumerate() {
            let (did, exists) = db
                .resolve_ingest_drawer_id(&request.wing, room, chunk, project_id.as_deref())
                .map_err(db_error)?;
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
                system_warnings: current_system_warnings(),
            }));
        }

        // Tier-1 gating runs on the full content (not per-chunk).
        let candidate = IngestCandidate {
            content: scrubbed_content.clone(),
            tool_name: None,
            exit_code: None,
        };
        let mut gating_decision = evaluate_tier1(&candidate, &config.ingest_gating);
        if let Some(decision) = gating_decision.as_ref()
            && decision.is_rejected()
        {
            db.record_gating_audit(&drawer_id, decision, project_id.as_deref())
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
                system_warnings: current_system_warnings(),
            }));
        }

        let mut db = db;

        // P9-B: acquire lock on the first chunk's drawer_id before embedding.
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

        // Tier-2 gating: uses the first chunk's embedding as representative.
        let first_chunk = chunks.first().map(|c| c.as_str()).unwrap_or("");
        let mut first_vector = None;
        let mut gating_audit_recorded = false;
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
                db.record_gating_audit(&drawer_id, &tier2.decision, project_id.as_deref())
                    .map_err(db_error)?;
                gating_audit_recorded = true;
                first_vector = tier2.vector;
                gating_decision = Some(tier2.decision);
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
                system_warnings: current_system_warnings(),
            }));
        }
        if !gating_audit_recorded && let Some(decision) = gating_decision.as_ref() {
            db.record_gating_audit(&drawer_id, decision, project_id.as_deref())
                .map_err(db_error)?;
        }

        // Embed ALL chunks in one batch call. If tier2 already produced a
        // vector for the first chunk, splice it in.
        let chunk_refs: Vec<&str> = chunks.iter().map(|c| c.as_str()).collect();
        let vectors = if first_vector.is_some() && chunks.len() == 1 {
            // Single-chunk optimization: reuse tier2 vector.
            vec![first_vector.take().expect("checked Some")]
        } else if let Some(fv) = first_vector.take() {
            // Multi-chunk with tier2 vector for first chunk: embed the rest.
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

        // Novelty evaluation uses the first chunk's vector as representative.
        let first_vector_ref = &vectors[0];
        let duplicate_warning = check_semantic_duplicate(&db, first_vector_ref, first_chunk);
        let novelty_candidate = NoveltyCandidate {
            wing: request.wing.clone(),
            room: request.room.clone(),
            project_id: project_id.clone(),
        };
        let novelty = evaluate_novelty(
            &db,
            &novelty_candidate,
            first_vector_ref,
            &config.ingest_gating.novelty,
        );
        let mut response_drawer_id = drawer_id.clone();
        let (novelty_action, near_drawer_id);

        // Collect all successfully inserted drawer IDs.
        let mut inserted_drawer_ids: Vec<String> = Vec::new();

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

                // Insert ALL chunks as separate drawers.
                for ((chunk_idx, chunk_did, chunk_exists), (chunk, vector)) in chunk_drawer_ids
                    .iter()
                    .zip(chunks.iter().zip(vectors.iter()))
                {
                    if *chunk_exists {
                        inserted_drawer_ids.push(chunk_did.clone());
                        continue;
                    }
                    // Acquire per-chunk lock for chunks beyond the first
                    // (first chunk already holds lock_guard).
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
                    // Re-check existence after acquiring per-chunk lock.
                    let exists_after_lock = if *chunk_idx > 0 {
                        db.drawer_exists(chunk_did).map_err(db_error)?
                    } else {
                        // First chunk: re-check after the initial lock.
                        db.drawer_exists(chunk_did).map_err(db_error)?
                    };
                    if exists_after_lock {
                        inserted_drawer_ids.push(chunk_did.clone());
                        continue;
                    }
                    let source_file =
                        source_file_or_synthetic(chunk_did, request.source.as_deref());
                    db.insert_drawer_with_project(
                        &Drawer {
                            id: chunk_did.clone(),
                            content: chunk.clone(),
                            wing: request.wing.clone(),
                            room: request.room.clone(),
                            source_file: Some(source_file),
                            source_type: SourceType::Manual,
                            added_at: current_timestamp(),
                            chunk_index: Some(*chunk_idx as i64),
                            importance: request.importance.unwrap_or(0),
                        },
                        project_id.as_deref(),
                    )
                    .map_err(db_error)?;
                    db.insert_vector_with_project(chunk_did, vector, project_id.as_deref())
                        .map_err(db_error)?;
                    inserted_drawer_ids.push(chunk_did.clone());
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
                // Novelty merge operates on the FULL scrubbed content (not
                // individual chunks). This preserves the existing merge
                // semantics: the whole content is appended as supplementary.
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
                        &mut inserted_drawer_ids,
                    )?;
                    novelty_action = Some(NoveltyAction::Insert);
                    near_drawer_id = Some(target_id);
                } else {
                    // Re-embed the merged content. Note: merged content may
                    // exceed max_input_tokens — this is intentional (existing
                    // merge semantics; the merged drawer is a single unit).
                    match embedder.embed(&[merged_content.as_str()]).await {
                        Ok(merged_vectors) => match merged_vectors.into_iter().next() {
                            Some(merged_vector) => {
                                ensure_vector_dim_matches(&db, merged_vector.len())?;
                                db.update_drawer_after_merge(
                                    &target_id,
                                    &merged_content,
                                    &merged_at,
                                    &merged_vector,
                                )
                                .map_err(db_error)?;
                                db.record_novelty_audit(
                                    &drawer_id,
                                    NoveltyAction::Merge,
                                    Some(target_id.as_str()),
                                    novelty.cosine,
                                    novelty.audit_decision,
                                    project_id.as_deref(),
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
                                    &mut inserted_drawer_ids,
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
                                &mut inserted_drawer_ids,
                            )?;
                            novelty_action = Some(NoveltyAction::Insert);
                            near_drawer_id = Some(target_id);
                        }
                    }
                }
            }
        }

        drop(lock_guard);

        if !inserted_drawer_ids.is_empty() {
            response_drawer_id = inserted_drawer_ids[0].clone();
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
            system_warnings: current_system_warnings(),
        }))
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
        description = "Discover cross-wing tunnels: rooms that appear in multiple wings, enabling cross-domain knowledge discovery. Returns an empty list if only one wing exists."
    )]
    async fn mempal_tunnels(&self) -> std::result::Result<Json<TunnelsResponse>, ErrorData> {
        let db = self.open_db()?;
        let tunnels = db
            .find_tunnels()
            .map_err(db_error)?
            .into_iter()
            .map(|(room, wings)| TunnelDto { room, wings })
            .collect();
        Ok(Json(TunnelsResponse {
            tunnels,
            system_warnings: current_system_warnings(),
        }))
    }

    #[tool(
        name = "mempal_peek_partner",
        description = "Read the partner coding agent's LIVE session log (Claude Code ↔ Codex) without storing it in mempal. Returns the most recent user+assistant messages from their active session file. Use this for CURRENT partner state; use mempal_search for CRYSTALLIZED past decisions. Peek is a pure read — it never writes to mempal drawers. Pass tool=\"auto\" to infer the partner from MCP ClientInfo, or tool=\"claude\"/\"codex\" explicitly."
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
        description = "Proactively deliver a short handoff message to the PARTNER agent's inbox. \
                       Partner reads it at their next UserPromptSubmit hook, NOT real-time. \
                       Use for transient handoffs too important for mempal_peek_partner \
                       and too ephemeral for mempal_ingest. Max 8 KB per message; total inbox \
                       capped at 32 KB / 16 messages (InboxFull error means partner must drain). \
                       Pass target_tool=\"claude\"/\"codex\" explicitly, or omit to infer partner \
                       from MCP client identity. Self-push is rejected."
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
        name = "mempal_fact_check",
        description = "Detect contradictions in text against KG triples + known entities. \
                       Returns SimilarNameConflict (similar-name typos), RelationContradiction \
                       (incompatible predicate for same endpoints), and StaleFact (KG valid_to \
                       expired) issues. Pure read, zero LLM, zero network, deterministic. \
                       Call before ingesting decisions that assert relationships between named \
                       entities to catch typos or outdated assumptions early."
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

        Ok(Json(FactCheckResponse {
            issues: report.issues,
            checked_entities: report.checked_entities,
            kg_triples_scanned: report.kg_triples_scanned,
            system_warnings: current_system_warnings(),
        }))
    }
}

/// Return the current UTC timestamp in RFC 3339 format (seconds precision).
/// Matches the format used by P6 peek_partner messages.
fn current_rfc3339() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Use the same days_to_ymd+format_rfc3339 helpers as cowork::peek,
    // but we don't need to pull them in — format as a simple UTC timestamp.
    // Use the existing format_rfc3339 via SystemTime conversion.
    let secs = now;
    // Reuse cowork::peek::format_rfc3339 is pub; call it to stay consistent.
    crate::cowork::peek::format_rfc3339(UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MempalMcpServer {
    fn get_info(&self) -> ServerInfo {
        // MCP spec: `instructions` is auto-injected into the LLM system prompt
        // by most clients at connection time. Putting the memory protocol here
        // means every client (Claude Code, Codex, Cursor, Continue, ...) sees
        // it without needing to call any tool first. This is the primary
        // mechanism; `mempal_status` keeps the same text as a fallback/reference.
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
        // Capture the calling client's tool name so `mempal_peek_partner`
        // with `tool: "auto"` can infer which partner to read (e.g.,
        // caller=claude-code ⇒ peek codex; caller=codex-cli ⇒ peek claude).
        if let Ok(mut guard) = self.client_name.lock() {
            *guard = Some(request.client_info.name.clone());
        }
        // Preserve rmcp's default behavior: store peer_info so downstream
        // rmcp internals can read client capabilities.
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
    let data = serde_json::to_value(&warnings).ok();
    ErrorData::internal_error(message, data)
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

fn read_drawer_response(details: crate::core::types::DrawerDetails) -> ReadDrawerResponse {
    let signals = crate::aaak::analyze(&details.drawer.content);
    let original_content_bytes = details.drawer.content.len() as u64;
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::mpsc;
    use std::time::Duration;

    use async_trait::async_trait;
    use tempfile::TempDir;

    use super::*;
    use crate::embed::Embedder;

    #[derive(Clone)]
    struct StubEmbedderFactory {
        vector: Vec<f32>,
    }

    struct StubEmbedder {
        vector: Vec<f32>,
    }

    #[derive(Clone)]
    struct HoldEmbedderFactory {
        vector: Vec<f32>,
        delay: Duration,
        entered: Arc<Mutex<Option<mpsc::Sender<()>>>>,
    }

    struct HoldEmbedder {
        vector: Vec<f32>,
        delay: Duration,
        entered: Arc<Mutex<Option<mpsc::Sender<()>>>>,
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
    impl crate::embed::EmbedderFactory for HoldEmbedderFactory {
        async fn build(&self) -> crate::embed::Result<Box<dyn Embedder>> {
            Ok(Box::new(HoldEmbedder {
                vector: self.vector.clone(),
                delay: self.delay,
                entered: Arc::clone(&self.entered),
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
    impl Embedder for HoldEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            if let Some(tx) = self.entered.lock().unwrap().take() {
                let _ = tx.send(());
            }
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            Ok(texts.iter().map(|_| self.vector.clone()).collect())
        }

        fn dimensions(&self) -> usize {
            self.vector.len()
        }

        fn name(&self) -> &str {
            "hold"
        }
    }

    fn setup_server() -> (TempDir, PathBuf, MempalMcpServer) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let server = MempalMcpServer::new_with_factory(
            db_path.clone(),
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        );
        (tempdir, db_path, server)
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
        db.insert_drawer_with_project(
            &Drawer {
                id: id.to_string(),
                content: content.to_string(),
                wing: wing.to_string(),
                room: room.map(str::to_string),
                source_file: Some(source_file.to_string()),
                source_type: SourceType::Manual,
                added_at: "1713000000".to_string(),
                chunk_index: Some(0),
                importance,
            },
            Some("default"),
        )
        .expect("insert drawer");
        db.insert_vector_with_project(id, &[0.1, 0.2, 0.3], Some("default"))
            .expect("insert vector");
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
                project_id: None,
                include_global: None,
                all_projects: None,
                disable_progressive: None,
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_mcp_ingest_response_exposes_lock_wait() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        Database::open(&db_path).expect("init db before concurrent open_db calls");
        let (entered_tx, entered_rx) = mpsc::channel();
        let server = Arc::new(MempalMcpServer::new_with_factory(
            db_path,
            Arc::new(HoldEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
                delay: Duration::from_millis(250),
                entered: Arc::new(Mutex::new(Some(entered_tx))),
            }),
        ));

        let request = IngestRequest {
            content: "same content for lock contention".to_string(),
            wing: "mempal".to_string(),
            room: Some("review".to_string()),
            source: None,
            project_id: None,
            importance: None,
            dry_run: None,
        };

        let server_a = Arc::clone(&server);
        let request_a = request.clone();
        let task_a =
            tokio::spawn(async move { server_a.mempal_ingest(Parameters(request_a)).await });
        entered_rx
            .recv()
            .expect("first ingest entered embed under lock");

        let server_b = Arc::clone(&server);
        let task_b = tokio::spawn(async move { server_b.mempal_ingest(Parameters(request)).await });

        let response_a = task_a
            .await
            .expect("join a")
            .expect("ingest a should succeed")
            .0;
        let response_b = task_b
            .await
            .expect("join b")
            .expect("ingest b should succeed")
            .0;

        let waits = [
            response_a.lock_wait_ms.unwrap_or(0),
            response_b.lock_wait_ms.unwrap_or(0),
        ];
        let waited = waits.into_iter().filter(|ms| *ms > 0).count();
        assert_eq!(waited, 1, "expected exactly one waiter: {waits:?}");

        let json = serde_json::to_value(&response_a).expect("serialize");
        assert!(
            json.get("lock_wait_ms").is_some(),
            "JSON must expose lock_wait_ms"
        );
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
}
