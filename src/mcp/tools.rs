use crate::core::types::{RouteDecision, SearchResult, TaxonomyEntry};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchRequest {
    /// Natural-language query. Use the user's actual question verbatim
    /// when possible — the embedding model handles paraphrase and translation.
    pub query: String,

    /// Optional wing filter. OMIT (leave null) unless you already know the
    /// EXACT wing name from a prior mempal_status call or the user named it
    /// explicitly. Wing filtering is a strict equality match, so guessing a
    /// wing name (e.g. "engineering", "backend") will silently return zero
    /// results. When in doubt, leave this field unset for a global search
    /// across all wings.
    pub wing: Option<String>,

    /// Optional room filter within a wing. Same rule as wing: OMIT unless you
    /// have seen the exact room name in a prior mempal_status call. Guessing
    /// returns zero results.
    pub room: Option<String>,

    /// Maximum number of results to return. Defaults to 10 when omitted.
    pub top_k: Option<usize>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResponse {
    pub results: Vec<SearchResultDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SearchResultDto {
    pub drawer_id: String,
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source_file: String,
    pub similarity: f32,
    pub route: RouteDecisionDto,
    /// Other wings sharing this room (tunnel cross-references).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tunnel_hints: Vec<String>,
    /// 3-4 letter entity codes derived from AAAK analysis.
    pub entities: Vec<String>,
    /// Topic keywords derived from AAAK analysis. May be empty.
    pub topics: Vec<String>,
    /// Classification flags derived from AAAK analysis. Always non-empty.
    pub flags: Vec<String>,
    /// Emotion tags derived from AAAK analysis. Always non-empty.
    pub emotions: Vec<String>,
    /// Importance derived from AAAK flags, normalized to the existing 2-4 scale.
    pub importance_stars: u8,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RouteDecisionDto {
    pub wing: Option<String>,
    pub room: Option<String>,
    pub confidence: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct IngestRequest {
    pub content: String,
    pub wing: String,
    pub room: Option<String>,
    pub source: Option<String>,

    /// If true, return the drawer_id that WOULD be created without actually
    /// writing to the database. Use this to preview before committing.
    pub dry_run: Option<bool>,

    /// Importance ranking (0-5). Higher values appear first in wake-up context.
    /// Default 0. Use 3-5 for key decisions, architecture choices, and lessons learned.
    pub importance: Option<i32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DeleteRequest {
    /// The drawer_id to soft-delete. The drawer is marked with a deleted_at
    /// timestamp but not physically removed. Use `mempal purge` CLI to
    /// permanently remove soft-deleted drawers.
    pub drawer_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DeleteResponse {
    pub drawer_id: String,
    pub deleted: bool,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct IngestResponse {
    pub drawer_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_warning: Option<DuplicateWarning>,
    /// Milliseconds spent waiting for the per-source ingest lock (P9-B).
    /// Omitted in dry-run and when lock was not acquired. When > 0, a
    /// concurrent ingest of the same content serialized with this call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_wait_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DuplicateWarning {
    pub similar_drawer_id: String,
    pub similarity: f32,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusResponse {
    pub schema_version: u32,
    pub drawer_count: i64,
    pub taxonomy_count: i64,
    pub db_size_bytes: u64,
    pub config_version: String,
    pub config_loaded_at_unix_ms: u64,
    pub scopes: Vec<ScopeCount>,
    pub aaak_spec: String,
    pub memory_protocol: String,
    pub embed_status: EmbedStatusDto,
    pub queue_stats: QueueStatsDto,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueStatsDto {
    pub pending: u64,
    pub claimed: u64,
    pub done: u64,
    pub failed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_pending_age_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmbedStatusDto {
    pub pending_count: u64,
    pub claimed_count: u64,
    pub failed_count: u64,
    pub degraded: bool,
    pub fail_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SystemWarning {
    pub level: String,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ScopeCount {
    pub wing: String,
    pub room: Option<String>,
    pub drawer_count: i64,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TaxonomyRequest {
    pub action: String,
    pub wing: Option<String>,
    pub room: Option<String>,
    pub keywords: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaxonomyResponse {
    pub action: String,
    pub entries: Vec<TaxonomyEntryDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TaxonomyEntryDto {
    pub wing: String,
    pub room: String,
    pub display_name: Option<String>,
    pub keywords: Vec<String>,
}

// --- Knowledge Graph ---

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct KgRequest {
    /// Action: "add", "query", or "invalidate".
    pub action: String,
    pub subject: Option<String>,
    pub predicate: Option<String>,
    pub object: Option<String>,
    /// Triple ID (required for invalidate).
    pub triple_id: Option<String>,
    /// Only return currently-valid triples (default true).
    pub active_only: Option<bool>,
    /// Link to the source drawer that evidences this triple.
    pub source_drawer: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KgResponse {
    pub action: String,
    pub triples: Vec<TripleDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<KgStatsDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct KgStatsDto {
    pub total: i64,
    pub active: i64,
    pub expired: i64,
    pub entities: i64,
    pub top_predicates: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TripleDto {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub confidence: f64,
    pub source_drawer: Option<String>,
}

// --- Tunnels ---

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TunnelsResponse {
    pub tunnels: Vec<TunnelDto>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TunnelDto {
    pub room: String,
    pub wings: Vec<String>,
}

// --- Cowork peek ---

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct PeekPartnerRequest {
    /// Which agent tool's session to read. "auto" uses MCP ClientInfo.name
    /// to infer the partner (Claude ↔ Codex); "claude" or "codex" bypasses
    /// inference. If you explicitly name your own tool the call is rejected
    /// to prevent self-peek.
    pub tool: String,

    /// Maximum number of user+assistant messages to return. Default 30.
    pub limit: Option<usize>,

    /// Optional RFC3339 timestamp cutoff — only messages strictly newer than
    /// this are returned.
    pub since: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PeekPartnerResponse {
    pub partner_tool: String,
    pub session_path: Option<String>,
    pub session_mtime: Option<String>,
    pub partner_active: bool,
    pub messages: Vec<PeekMessageDto>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PeekMessageDto {
    pub role: String,
    pub at: String,
    pub text: String,
}

impl From<crate::cowork::PeekMessage> for PeekMessageDto {
    fn from(m: crate::cowork::PeekMessage) -> Self {
        Self {
            role: m.role,
            at: m.at,
            text: m.text,
        }
    }
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CoworkPushRequest {
    /// The message content to deliver. Maximum 8 KB. Short status updates,
    /// decision summaries, or drawer_id pointers. Do NOT push search results
    /// or large reasoning blocks — see Rule 10 in MEMORY_PROTOCOL.
    pub content: String,

    /// Target agent: "claude" or "codex". OMIT to infer partner from MCP
    /// client identity (Claude → Codex, Codex → Claude). Self-push is rejected.
    #[serde(default)]
    pub target_tool: Option<String>,

    /// Absolute filesystem path of the project cwd this push is scoped to.
    /// Internally normalized to git repo root via `project_identity()` so
    /// subdirectory callers land on the same inbox as repo-root callers.
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CoworkPushResponse {
    pub target_tool: String,
    pub inbox_path: String,
    pub pushed_at: String,
    pub inbox_size_after: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FactCheckRequest {
    /// Text to check for contradictions against KG triples + known entities.
    pub text: String,
    /// Optional wing filter for known-entity scope. OMIT unless you have
    /// already seen the exact wing name via mempal_status.
    pub wing: Option<String>,
    /// Optional room filter within a wing. OMIT unless explicitly named.
    pub room: Option<String>,
    /// Optional RFC3339 timestamp for the `now` cutoff used by
    /// StaleFact detection. OMIT to use current UTC time.
    pub now: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FactCheckResponse {
    pub issues: Vec<crate::factcheck::FactIssue>,
    pub checked_entities: Vec<String>,
    pub kg_triples_scanned: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system_warnings: Vec<SystemWarning>,
}

impl SearchResultDto {
    pub fn with_signals_from_result(value: SearchResult) -> Self {
        let signals = crate::aaak::analyze(&value.content);

        Self {
            drawer_id: value.drawer_id,
            content: value.content,
            wing: value.wing,
            room: value.room,
            source_file: value.source_file,
            similarity: value.similarity,
            route: value.route.into(),
            tunnel_hints: value.tunnel_hints,
            entities: signals.entities,
            topics: signals.topics,
            flags: signals.flags,
            emotions: signals.emotions,
            importance_stars: signals.importance_stars,
        }
    }
}

impl From<RouteDecision> for RouteDecisionDto {
    fn from(value: RouteDecision) -> Self {
        Self {
            wing: value.wing,
            room: value.room,
            confidence: value.confidence,
            reason: value.reason,
        }
    }
}

impl From<TaxonomyEntry> for TaxonomyEntryDto {
    fn from(value: TaxonomyEntry) -> Self {
        Self {
            wing: value.wing,
            room: value.room,
            display_name: value.display_name,
            keywords: value.keywords,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::core::types::{RouteDecision, SearchResult};

    use super::SearchResultDto;

    fn sample_result(content: &str) -> SearchResult {
        SearchResult {
            drawer_id: "drawer-1".to_string(),
            content: content.to_string(),
            wing: "mempal".to_string(),
            room: Some("signals".to_string()),
            source_file: "/tmp/signals.md".to_string(),
            similarity: 0.91,
            route: RouteDecision {
                wing: Some("mempal".to_string()),
                room: Some("signals".to_string()),
                confidence: 0.88,
                reason: "unit test".to_string(),
            },
            tunnel_hints: vec!["docs".to_string()],
        }
    }

    #[test]
    fn test_with_signals_preserves_raw_content_and_citations() {
        let original = "We decided to use Arc<Mutex<>> for state because shared ownership mattered";
        let dto = SearchResultDto::with_signals_from_result(sample_result(original));

        assert_eq!(dto.content, original);
        assert!(!dto.content.starts_with("V1|"));
        assert!(!dto.content.contains('★'));
        assert_eq!(dto.drawer_id, "drawer-1");
        assert_eq!(dto.source_file, "/tmp/signals.md");
        assert_eq!(dto.tunnel_hints, vec!["docs".to_string()]);
        assert!(dto.flags.contains(&"DECISION".to_string()));
        assert!(dto.importance_stars >= 2);
        assert!(!dto.entities.is_empty());
    }

    #[test]
    fn test_with_signals_applies_empty_content_sentinels() {
        let dto = SearchResultDto::with_signals_from_result(sample_result(""));

        assert_eq!(dto.entities, vec!["UNK".to_string()]);
        assert_eq!(dto.flags, vec!["CORE".to_string()]);
        assert_eq!(dto.emotions, vec!["determ".to_string()]);
        assert!(dto.topics.is_empty());
        assert_eq!(dto.importance_stars, 2);
    }
}
