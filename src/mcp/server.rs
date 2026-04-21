use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::core::{
    anchor::{self, DerivedAnchor},
    db::Database,
    types::{
        AnchorKind, Drawer, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, Provenance,
        SourceType, TriggerHints, Triple,
    },
    utils::{
        build_bootstrap_drawer_id, build_triple_id, current_timestamp, knowledge_source_file,
        source_file_or_synthetic,
    },
};
use crate::cowork::{PeekError, PeekRequest as CoworkPeekRequest, Tool, peek_partner};
use crate::embed::EmbedderFactory;
use crate::search::{resolve_route, search_with_vector};
use anyhow::Context;
use rmcp::{
    ErrorData, Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
};
use serde_json::Value;

use super::tools::{
    CoworkPushRequest, CoworkPushResponse, DeleteRequest, DeleteResponse, DuplicateWarning,
    FactCheckRequest, FactCheckResponse, IngestRequest, IngestResponse, KgRequest, KgResponse,
    KgStatsDto, PeekMessageDto, PeekPartnerRequest, PeekPartnerResponse, ScopeCount, SearchRequest,
    SearchResponse, SearchResultDto, StatusResponse, TaxonomyEntryDto, TaxonomyRequest,
    TaxonomyResponse, TriggerHintsDto, TripleDto, TunnelDto, TunnelsResponse,
};

#[derive(Clone)]
pub struct MempalMcpServer {
    db_path: PathBuf,
    embedder_factory: Arc<dyn EmbedderFactory>,
    tool_router: ToolRouter<Self>,
    /// Captured via `initialize` override so `auto` peek mode can infer the
    /// partner from the calling MCP client's self-reported name.
    client_name: Arc<Mutex<Option<String>>>,
}

impl MempalMcpServer {
    pub fn new(db_path: PathBuf, config: crate::core::config::Config) -> Self {
        Self::new_with_factory(
            db_path,
            Arc::new(crate::embed::ConfiguredEmbedderFactory::new(config)),
        )
    }

    pub fn new_with_factory(db_path: PathBuf, embedder_factory: Arc<dyn EmbedderFactory>) -> Self {
        Self {
            db_path,
            embedder_factory,
            tool_router: Self::tool_router(),
            client_name: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn serve_stdio(
        self,
    ) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleServer, Self>> {
        self.serve(rmcp::transport::stdio())
            .await
            .context("failed to initialize MCP stdio transport")
    }

    fn open_db(&self) -> std::result::Result<Database, ErrorData> {
        Database::open(&self.db_path).map_err(|error| {
            ErrorData::internal_error(format!("failed to open database: {error}"), None)
        })
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
}

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

impl ValidatedIngestMetadata {
    fn identity_components(&self) -> Vec<String> {
        let mut components = vec![
            format!("memory_kind={}", memory_kind_slug(&self.memory_kind)),
            format!("domain={}", domain_slug(&self.domain)),
            format!("field={}", self.field),
            format!("anchor_kind={}", anchor_kind_slug(&self.anchor_kind)),
            format!("anchor_id={}", self.anchor_id),
            format!(
                "parent_anchor_id={}",
                self.parent_anchor_id.as_deref().unwrap_or("")
            ),
            format!(
                "provenance={}",
                self.provenance.as_ref().map(provenance_slug).unwrap_or("")
            ),
            format!("statement={}", self.statement.as_deref().unwrap_or("")),
            format!(
                "tier={}",
                self.tier.as_ref().map(knowledge_tier_slug).unwrap_or("")
            ),
            format!(
                "status={}",
                self.status
                    .as_ref()
                    .map(knowledge_status_slug)
                    .unwrap_or("")
            ),
            format!(
                "scope_constraints={}",
                self.scope_constraints.as_deref().unwrap_or("")
            ),
            format!("supporting_refs={}", self.supporting_refs.join(",")),
            format!("counterexample_refs={}", self.counterexample_refs.join(",")),
            format!("teaching_refs={}", self.teaching_refs.join(",")),
            format!("verification_refs={}", self.verification_refs.join(",")),
        ];

        if let Some(trigger_hints) = &self.trigger_hints {
            components.push(format!(
                "intent_tags={}",
                trigger_hints.intent_tags.join(",")
            ));
            components.push(format!(
                "workflow_bias={}",
                trigger_hints.workflow_bias.join(",")
            ));
            components.push(format!("tool_needs={}", trigger_hints.tool_needs.join(",")));
        }

        components
    }
}

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
            validate_supporting_refs(&supporting_refs)?;

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

fn validate_anchor_metadata(
    request: &IngestRequest,
    domain: &MemoryDomain,
    source_type: &SourceType,
) -> std::result::Result<DerivedAnchor, ErrorData> {
    let explicit_kind = trim_to_option(request.anchor_kind.as_deref());
    let explicit_id = trim_to_option(request.anchor_id.as_deref());

    let anchor = match (explicit_kind, explicit_id) {
        (Some(kind), Some(anchor_id)) => DerivedAnchor {
            anchor_kind: parse_anchor_kind(Some(kind))?.expect("explicit kind"),
            anchor_id: anchor_id.to_string(),
            parent_anchor_id: trim_to_owned(request.parent_anchor_id.as_deref()),
        },
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

fn parse_anchor_kind(value: Option<&str>) -> std::result::Result<Option<AnchorKind>, ErrorData> {
    parse_enum(value, "anchor_kind", |normalized| match normalized {
        "global" => Some(AnchorKind::Global),
        "repo" => Some(AnchorKind::Repo),
        "worktree" => Some(AnchorKind::Worktree),
        _ => None,
    })
}

fn parse_provenance(value: Option<&str>) -> std::result::Result<Option<Provenance>, ErrorData> {
    parse_enum(value, "provenance", |normalized| match normalized {
        "runtime" => Some(Provenance::Runtime),
        "research" => Some(Provenance::Research),
        "human" => Some(Provenance::Human),
        _ => None,
    })
}

fn parse_tier(value: Option<&str>) -> std::result::Result<Option<KnowledgeTier>, ErrorData> {
    parse_enum(value, "tier", |normalized| match normalized {
        "qi" => Some(KnowledgeTier::Qi),
        "shu" => Some(KnowledgeTier::Shu),
        "dao_ren" => Some(KnowledgeTier::DaoRen),
        "dao_tian" => Some(KnowledgeTier::DaoTian),
        _ => None,
    })
}

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

fn normalize_refs(values: Option<&[String]>) -> Vec<String> {
    values
        .unwrap_or(&[])
        .iter()
        .filter_map(|value| trim_to_owned(Some(value.as_str())))
        .collect()
}

fn validate_supporting_refs(values: &[String]) -> std::result::Result<(), ErrorData> {
    if values.iter().all(|value| looks_like_drawer_id(value)) {
        Ok(())
    } else {
        Err(ErrorData::invalid_params(
            "supporting_refs must contain drawer ids",
            None,
        ))
    }
}

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

fn anchor_error(error: anchor::AnchorError) -> ErrorData {
    ErrorData::invalid_params(error.to_string(), None)
}

fn memory_kind_slug(value: &MemoryKind) -> &'static str {
    match value {
        MemoryKind::Evidence => "evidence",
        MemoryKind::Knowledge => "knowledge",
    }
}

fn domain_slug(value: &MemoryDomain) -> &'static str {
    match value {
        MemoryDomain::Project => "project",
        MemoryDomain::Agent => "agent",
        MemoryDomain::Skill => "skill",
        MemoryDomain::Global => "global",
    }
}

fn anchor_kind_slug(value: &AnchorKind) -> &'static str {
    match value {
        AnchorKind::Global => "global",
        AnchorKind::Repo => "repo",
        AnchorKind::Worktree => "worktree",
    }
}

fn provenance_slug(value: &Provenance) -> &'static str {
    match value {
        Provenance::Runtime => "runtime",
        Provenance::Research => "research",
        Provenance::Human => "human",
    }
}

fn knowledge_tier_slug(value: &KnowledgeTier) -> &'static str {
    match value {
        KnowledgeTier::Qi => "qi",
        KnowledgeTier::Shu => "shu",
        KnowledgeTier::DaoRen => "dao_ren",
        KnowledgeTier::DaoTian => "dao_tian",
    }
}

fn knowledge_status_slug(value: &KnowledgeStatus) -> &'static str {
    match value {
        KnowledgeStatus::Candidate => "candidate",
        KnowledgeStatus::Promoted => "promoted",
        KnowledgeStatus::Canonical => "canonical",
        KnowledgeStatus::Demoted => "demoted",
        KnowledgeStatus::Retired => "retired",
    }
}

#[tool_router(router = tool_router)]
impl MempalMcpServer {
    #[tool(
        name = "mempal_status",
        description = "Return schema version, drawer counts, taxonomy counts, database size, scope breakdown, the AAAK format spec, and the memory protocol. Call once at session start if you haven't seen the protocol yet."
    )]
    async fn mempal_status(&self) -> std::result::Result<Json<StatusResponse>, ErrorData> {
        let db = self.open_db()?;
        let schema_version = db.schema_version().map_err(db_error)?;
        let drawer_count = db.drawer_count().map_err(db_error)?;
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

        Ok(Json(StatusResponse {
            schema_version,
            drawer_count,
            taxonomy_count,
            db_size_bytes,
            scopes,
            aaak_spec: crate::aaak::generate_spec(),
            memory_protocol: crate::core::protocol::MEMORY_PROTOCOL.to_string(),
        }))
    }

    #[tool(
        name = "mempal_search",
        description = "Search persistent project memory via vector embedding with optional wing/room filters. PREFER THIS over grepping files or guessing from general knowledge when answering ANY project-specific question — past decisions, design rationale, implementation details, bug history, how a component works, why something was built a certain way, or any other project knowledge. Every result includes drawer_id and source_file for citation, plus structured AAAK-derived signals (`entities`, `topics`, `flags`, `emotions`, `importance_stars`) for filtering and ranking."
    )]
    async fn mempal_search(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> std::result::Result<Json<SearchResponse>, ErrorData> {
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
        let route = resolve_route(
            &db,
            &request.query,
            request.wing.as_deref(),
            request.room.as_deref(),
        )
        .map_err(|error| ErrorData::internal_error(format!("routing failed: {error}"), None))?;
        let results = search_with_vector(
            &db,
            &request.query,
            &query_vector,
            route,
            request.top_k.unwrap_or(10),
        )
        .map_err(|error| ErrorData::internal_error(format!("search failed: {error}"), None))?;

        Ok(Json(SearchResponse {
            results: results
                .into_iter()
                .map(SearchResultDto::with_signals_from_result)
                .collect(),
        }))
    }

    #[tool(
        name = "mempal_ingest",
        description = "Persist a decision, bug fix, or design insight to project memory. Call this when a decision is reached in conversation — include the rationale, not just the outcome. Wing is required; let mempal auto-route the room. Set dry_run=true to preview the drawer_id without writing."
    )]
    async fn mempal_ingest(
        &self,
        Parameters(request): Parameters<IngestRequest>,
    ) -> std::result::Result<Json<IngestResponse>, ErrorData> {
        let room = request.room.as_deref();
        let metadata = validate_ingest_request(&request, &SourceType::Manual)?;
        let drawer_id = build_bootstrap_drawer_id(
            &request.wing,
            room,
            &request.content,
            &metadata.identity_components(),
        );

        if request.dry_run.unwrap_or(false) {
            return Ok(Json(IngestResponse {
                drawer_id,
                duplicate_warning: None,
                lock_wait_ms: None,
            }));
        }

        let embedder = self.embedder_factory.build().await.map_err(|error| {
            ErrorData::internal_error(format!("failed to build embedder: {error}"), None)
        })?;
        let vector = embedder
            .embed(&[request.content.as_str()])
            .await
            .map_err(|error| ErrorData::internal_error(format!("embedding failed: {error}"), None))?
            .into_iter()
            .next()
            .ok_or_else(|| ErrorData::internal_error("embedder returned no vector", None))?;
        let db = self.open_db()?;

        // P9-B: per-source ingest lock guards the dedup/insert critical
        // section. Lock key derives from the drawer_id (content-addressed,
        // filesystem-safe). Two concurrent mempal_ingest calls with the
        // same content serialize here.
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

        // Semantic dedup check: find most similar existing drawer
        let duplicate_warning = check_semantic_duplicate(&db, &vector, &request.content);

        if !db.drawer_exists(&drawer_id).map_err(db_error)? {
            let source_file = match metadata.memory_kind {
                MemoryKind::Evidence => {
                    source_file_or_synthetic(&drawer_id, request.source.as_deref())
                }
                MemoryKind::Knowledge => knowledge_source_file(
                    &metadata.domain,
                    &metadata.field,
                    metadata.tier.as_ref().expect("knowledge tier validated"),
                    metadata
                        .statement
                        .as_deref()
                        .expect("knowledge statement validated"),
                ),
            };
            let drawer = Drawer {
                id: drawer_id.clone(),
                content: request.content,
                wing: request.wing,
                room: request.room,
                source_file: Some(source_file),
                source_type: SourceType::Manual,
                added_at: current_timestamp(),
                chunk_index: Some(0),
                importance: request.importance.unwrap_or(0),
                memory_kind: metadata.memory_kind,
                domain: metadata.domain,
                field: metadata.field,
                anchor_kind: metadata.anchor_kind,
                anchor_id: metadata.anchor_id,
                parent_anchor_id: metadata.parent_anchor_id,
                provenance: metadata.provenance,
                statement: metadata.statement,
                tier: metadata.tier,
                status: metadata.status,
                supporting_refs: metadata.supporting_refs,
                counterexample_refs: metadata.counterexample_refs,
                teaching_refs: metadata.teaching_refs,
                verification_refs: metadata.verification_refs,
                scope_constraints: metadata.scope_constraints,
                trigger_hints: metadata.trigger_hints,
            };
            db.insert_drawer(&drawer).map_err(db_error)?;
            db.insert_vector(&drawer_id, &vector).map_err(db_error)?;
        }

        // lock_guard drops here, releasing the advisory lock.
        drop(lock_guard);

        Ok(Json(IngestResponse {
            drawer_id,
            duplicate_warning,
            lock_wait_ms,
        }))
    }

    #[tool(
        name = "mempal_delete",
        description = "Soft-delete a drawer by ID. The drawer is marked with a deleted_at timestamp and excluded from search results, but not physically removed. Use the CLI `mempal purge` to permanently remove soft-deleted drawers. Returns the drawer_id and whether it was found."
    )]
    async fn mempal_delete(
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
        }))
    }

    #[tool(
        name = "mempal_taxonomy",
        description = "List or edit wing/room taxonomy entries that drive query routing keywords."
    )]
    async fn mempal_taxonomy(
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
    async fn mempal_kg(
        &self,
        Parameters(request): Parameters<KgRequest>,
    ) -> std::result::Result<Json<KgResponse>, ErrorData> {
        let db = self.open_db()?;
        match request.action.as_str() {
            "add" => {
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
                }))
            }
            "invalidate" => {
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
        Ok(Json(TunnelsResponse { tunnels }))
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(crate::core::protocol::MEMORY_PROTOCOL)
    }

    fn initialize(
        &self,
        request: rmcp::model::InitializeRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl std::future::Future<
        Output = std::result::Result<rmcp::model::InitializeResult, ErrorData>,
    > + Send
    + '_ {
        // Capture the calling client's tool name so `mempal_peek_partner`
        // with `tool: "auto"` can infer which partner to read (e.g.,
        // caller=claude-code ⇒ peek codex; caller=codex-cli ⇒ peek claude).
        if let Ok(mut guard) = self.client_name.lock() {
            *guard = Some(request.client_info.name.clone());
        }
        // Preserve rmcp's default behavior: store peer_info so downstream
        // rmcp internals can read client capabilities.
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        std::future::ready(Ok(self.get_info()))
    }
}

fn db_error(error: impl std::fmt::Display) -> ErrorData {
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
    let results = crate::search::search_by_vector(db, vector, route, 1).ok()?;
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
    use crate::core::types::BootstrapEvidenceArgs;
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
        db.insert_drawer(&Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: wing.to_string(),
            room: room.map(str::to_string),
            source_file: Some(source_file.to_string()),
            source_type: SourceType::Manual,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance,
        }))
        .expect("insert drawer");
        db.insert_vector(id, &[0.1, 0.2, 0.3])
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
            importance: None,
            dry_run: None,
            memory_kind: None,
            domain: None,
            field: None,
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
