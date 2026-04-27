use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::context::assemble_context_with_vector;
use crate::core::{
    anchor::{self, DerivedAnchor},
    config::ConfigHandle,
    db::Database,
    project::{ProjectSearchScope, infer_project_id_from_root_uri, validate_project_id},
    types::{
        AnchorKind, BootstrapIdentityParts, Drawer, ExplicitTunnel, KnowledgeStatus, KnowledgeTier,
        MemoryDomain, MemoryKind, Provenance, SourceType, TriggerHints, Triple,
    },
    utils::{
        build_bootstrap_drawer_id_from_parts, build_triple_id, current_timestamp, iso_timestamp,
        knowledge_source_file, normalize_rfc3339_timestamp, source_file_or_synthetic,
    },
};
use crate::cowork::{PeekError, PeekRequest as CoworkPeekRequest, Tool, peek_partner};
use crate::embed::{EmbedderFactory, global_embed_status};
use crate::field_taxonomy::field_taxonomy;
use crate::ingest::{
    IngestError,
    gating::{GatingDecision, GatingRuntime, IngestCandidate, evaluate_tier1, evaluate_tier2},
    normalize::CURRENT_NORMALIZE_VERSION,
    novelty::{NoveltyAction, NoveltyCandidate, evaluate as evaluate_novelty},
};
use crate::knowledge_anchor::{PublishAnchorRequest as CorePublishAnchorRequest, publish_anchor};
use crate::knowledge_distill::{
    DistillPlan, DistillRequest as CoreDistillRequest, commit_distill, prepare_distill,
};
use crate::knowledge_gate::{evaluate_gate_by_id, promotion_policy};
use crate::knowledge_lifecycle::{
    DemoteRequest as CoreDemoteRequest, PromoteRequest as CorePromoteRequest, demote_knowledge,
    promote_knowledge,
};
use crate::search::{
    SearchFilters, SearchOptions, resolve_route, search_bm25_only,
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
use serde_json::Value;

use super::timeline::{TimelineRequest, TimelineResponse};
use super::tools::{
    ChunkerStatsDto, ContextRequest, ContextResponse, CoworkPushRequest, CoworkPushResponse,
    DeleteRequest, DeleteResponse, DuplicateWarning, EmbedStatusDto, FactCheckRequest,
    FactCheckResponse, FieldTaxonomyEntryDto, FieldTaxonomyResponse, IngestRequest, IngestResponse,
    KgRequest, KgResponse, KgStatsDto, KnowledgeDemoteRequest, KnowledgeDemoteResponse,
    KnowledgeDistillRequest, KnowledgeDistillResponse, KnowledgeGateRequest, KnowledgeGateResponse,
    KnowledgePolicyResponse, KnowledgePromoteRequest, KnowledgePromoteResponse,
    KnowledgePublishAnchorRequest, KnowledgePublishAnchorResponse, MAX_READ_DRAWERS_MAX_COUNT,
    MAX_READ_DRAWERS_REQUEST_IDS, PeekMessageDto, PeekPartnerRequest, PeekPartnerResponse,
    QueueStatsDto, ReadDrawerRequest, ReadDrawerResponse, ReadDrawersRequest, ReadDrawersResponse,
    RollbackRequest, RollbackResponse, ScopeCount, ScrubStatsDto, SearchRequest, SearchResponse,
    SearchResultDto, StatusResponse, SystemWarning, TaxonomyEntryDto, TaxonomyRequest,
    TaxonomyResponse, TriggerHintsDto, TripleDto, TunnelDto, TunnelEndpointDto, TunnelsRequest,
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

    pub async fn ingest_json_for_test(
        &self,
        value: Value,
    ) -> std::result::Result<IngestResponse, ErrorData> {
        let request = serde_json::from_value(value)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.mempal_ingest(Parameters(request))
            .await
            .map(|response| response.0)
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

    pub async fn knowledge_policy_json_for_test(
        &self,
    ) -> std::result::Result<KnowledgePolicyResponse, ErrorData> {
        self.mempal_knowledge_policy()
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
        let source_type = SourceType::Manual;
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
                inserted_drawer_ids.push(chunk_did.clone());
                continue;
            }
            let drawer = drawer_from_ingest_metadata(
                request,
                &metadata,
                chunk_did,
                chunk,
                *chunk_idx,
                &source_type,
            );
            db.insert_drawer_with_project(&drawer, project_id)
                .map_err(db_error)?;
            db.insert_vector_with_project(chunk_did, vector, project_id)
                .map_err(db_error)?;
            inserted_drawer_ids.push(chunk_did.clone());
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
#[derive(Debug)]
struct ValidatedIngestMetadata {
    memory_kind: MemoryKind,
    domain: MemoryDomain,
    field: String,
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

#[allow(dead_code)]
fn validate_ingest_request(
    request: &IngestRequest,
    source_type: &SourceType,
) -> std::result::Result<ValidatedIngestMetadata, ErrorData> {
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
                || status.is_some()
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

            Ok(ValidatedIngestMetadata {
                memory_kind,
                domain,
                field,
                anchor_kind: derived_anchor.anchor_kind,
                anchor_id: derived_anchor.anchor_id,
                parent_anchor_id: derived_anchor.parent_anchor_id,
                provenance: Some(
                    provenance.unwrap_or_else(|| anchor::bootstrap_provenance(source_type)),
                ),
                statement: None,
                tier: None,
                status: None,
                supporting_refs: Vec::new(),
                counterexample_refs: Vec::new(),
                teaching_refs: Vec::new(),
                verification_refs: Vec::new(),
                scope_constraints: None,
                trigger_hints: None,
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

fn drawer_from_ingest_metadata(
    request: &IngestRequest,
    metadata: &ValidatedIngestMetadata,
    drawer_id: &str,
    content: &str,
    chunk_idx: usize,
    source_type: &SourceType,
) -> Drawer {
    let source_file = match metadata.memory_kind {
        MemoryKind::Knowledge => Some(knowledge_source_file(
            &metadata.domain,
            &metadata.field,
            metadata.tier.as_ref().expect("validated knowledge tier"),
            metadata
                .statement
                .as_deref()
                .expect("validated knowledge statement"),
        )),
        MemoryKind::Evidence => Some(source_file_or_synthetic(
            drawer_id,
            request.source.as_deref(),
        )),
    };

    Drawer {
        id: drawer_id.to_string(),
        content: content.to_string(),
        wing: request.wing.clone(),
        room: request.room.clone(),
        source_file,
        source_type: source_type.clone(),
        added_at: iso_timestamp(),
        chunk_index: Some(chunk_idx as i64),
        normalize_version: CURRENT_NORMALIZE_VERSION,
        importance: request.importance.unwrap_or(0),
        memory_kind: metadata.memory_kind.clone(),
        domain: metadata.domain.clone(),
        field: metadata.field.clone(),
        anchor_kind: metadata.anchor_kind.clone(),
        anchor_id: metadata.anchor_id.clone(),
        parent_anchor_id: metadata.parent_anchor_id.clone(),
        provenance: metadata.provenance.clone(),
        statement: metadata.statement.clone(),
        tier: metadata.tier.clone(),
        status: metadata.status.clone(),
        supporting_refs: metadata.supporting_refs.clone(),
        counterexample_refs: metadata.counterexample_refs.clone(),
        teaching_refs: metadata.teaching_refs.clone(),
        verification_refs: metadata.verification_refs.clone(),
        scope_constraints: metadata.scope_constraints.clone(),
        trigger_hints: metadata.trigger_hints.clone(),
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
            KnowledgeStatus::Candidate,
            KnowledgeStatus::Promoted,
            KnowledgeStatus::Demoted,
            KnowledgeStatus::Retired,
        ][..],
        KnowledgeTier::Shu => &[
            KnowledgeStatus::Promoted,
            KnowledgeStatus::Demoted,
            KnowledgeStatus::Retired,
        ][..],
        KnowledgeTier::Qi => &[
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
    parse_enum(value, "memory_kind", |normalized| match normalized {
        "evidence" => Some(MemoryKind::Evidence),
        "knowledge" => Some(MemoryKind::Knowledge),
        _ => None,
    })
}

fn parse_domain(value: Option<&str>) -> std::result::Result<Option<MemoryDomain>, ErrorData> {
    parse_enum(value, "domain", |normalized| match normalized {
        "project" => Some(MemoryDomain::Project),
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

#[allow(dead_code)]
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
        let stale_drawer_count = db
            .stale_drawer_count(CURRENT_NORMALIZE_VERSION)
            .map_err(db_error)? as u64;
        let drawer_count = db.drawer_count().map_err(db_error)?;
        let null_project_backfill_pending =
            db.null_project_backfill_pending_count().map_err(db_error)?;
        let taxonomy_count = db.taxonomy_count().map_err(db_error)?;
        let db_size_bytes = db.database_size_bytes().map_err(db_error)?;
        let diary_rollup_days = db.diary_rollup_days().map_err(db_error)?;
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
            normalize_version_current: CURRENT_NORMALIZE_VERSION,
            stale_drawer_count,
            drawer_count,
            taxonomy_count,
            db_size_bytes,
            diary_rollup_days,
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
        let search_options = SearchOptions {
            filters: SearchFilters {
                memory_kind: request.memory_kind.clone(),
                domain: request.domain.clone(),
                field: request.field.clone(),
                tier: request.tier.clone(),
                status: request.status.clone(),
                anchor_kind: request.anchor_kind.clone(),
            },
            with_neighbors: request.with_neighbors.unwrap_or(false),
        };
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
                search_with_vector_and_scope_options(
                    &db,
                    &request.query,
                    &query_vector,
                    route.clone(),
                    &scope,
                    search_options.clone(),
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
        name = "mempal_context",
        description = "Assemble a mind-model runtime context pack from typed memory. Use this when you need ordered guidance rather than raw search results: dao_tian -> dao_ren -> shu -> qi, with evidence opt-in. Returns source-backed items with drawer_id/source_file citations and trigger_hints metadata, but never executes skills."
    )]
    async fn mempal_context(
        &self,
        Parameters(request): Parameters<ContextRequest>,
    ) -> std::result::Result<Json<ContextResponse>, ErrorData> {
        let max_items = request.max_items.unwrap_or(12);
        if max_items == 0 {
            return Err(ErrorData::invalid_params(
                "max_items must be greater than 0",
                None,
            ));
        }
        let dao_tian_limit = request.dao_tian_limit.unwrap_or(1);

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
        let pack = assemble_context_with_vector(
            &db,
            crate::context::ContextRequest {
                query: request.query,
                domain,
                field: request
                    .field
                    .unwrap_or_else(|| anchor::DEFAULT_FIELD.to_string()),
                cwd,
                include_evidence: request.include_evidence.unwrap_or(false),
                max_items,
                dao_tian_limit,
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
        let source_type = SourceType::Manual;
        let metadata = validate_ingest_request(&request, &source_type)?;

        if !dry_run && global_embed_status().should_block_writes() {
            return Err(degraded_write_error());
        }

        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let chunks =
            crate::ingest::prepare_chunks(&scrubbed_content, &config.chunker, embedder.as_ref());

        let mut chunk_drawer_ids: Vec<(usize, String, bool)> = Vec::with_capacity(chunks.len());
        for (idx, chunk) in chunks.iter().enumerate() {
            let did = build_bootstrap_drawer_id_from_parts(
                &request.wing,
                room,
                chunk,
                metadata.identity_parts(),
            );
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
                system_warnings: current_system_warnings(),
            }));
        }

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

                for ((chunk_idx, chunk_did, chunk_exists), (chunk, vector)) in chunk_drawer_ids
                    .iter()
                    .zip(chunks.iter().zip(vectors.iter()))
                {
                    if *chunk_exists {
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
                        inserted_drawer_ids.push(chunk_did.clone());
                        continue;
                    }
                    let drawer = drawer_from_ingest_metadata(
                        &request,
                        &metadata,
                        chunk_did,
                        chunk,
                        *chunk_idx,
                        &source_type,
                    );
                    db.insert_drawer_with_project(&drawer, project_id.as_deref())
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

        Ok(Json(FactCheckResponse {
            issues: report.issues,
            checked_entities: report.checked_entities,
            kg_triples_scanned: report.kg_triples_scanned,
            system_warnings: current_system_warnings(),
        }))
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
        | crate::context::ContextError::LoadDrawer(_) => {
            ErrorData::internal_error(format!("context assembly failed: {error}"), None)
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
