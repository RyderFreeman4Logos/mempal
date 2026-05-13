use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::aaak::{AaakSignals, analyze};
use crate::core::config::Config;
use crate::core::db::{Database, DbError};
use crate::core::decay::parse_temporal_timestamp_secs;
use crate::core::types::{
    Drawer, DrawerDetails, KnowledgeCard, KnowledgeCardEvent, KnowledgeEventType,
    KnowledgeEvidenceLink, KnowledgeEvidenceRole, KnowledgeStatus, KnowledgeTier,
};
use crate::core::utils::current_timestamp;
use crate::intelligence::global_intelligence_status;
use crate::llm::{LlmClient, LlmMessage, LlmRequest};

#[derive(Debug, Error)]
pub enum CrystallizeError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Llm(#[from] crate::llm::LlmError),
    #[error("invalid LLM crystallization draft: {0}")]
    InvalidLlmDraft(String),
}

pub type Result<T> = std::result::Result<T, CrystallizeError>;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrystallizeOptions {
    pub dry_run: bool,
    pub project_id: Option<String>,
    pub use_llm: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CrystallizeSummary {
    pub candidates_found: usize,
    pub cards_created: usize,
    pub dry_run: bool,
    pub used_llm: bool,
    pub fallback_count: usize,
    pub candidates: Vec<CrystallizeCandidate>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CrystallizeCandidate {
    pub cluster_key: String,
    pub drawer_count: usize,
    pub avg_importance: f64,
    pub avg_confidence: f64,
    pub time_span_days: f64,
    pub crystallization_score: f64,
    pub common_entities: Vec<String>,
    pub common_topics: Vec<String>,
    pub source_files: Vec<String>,
    pub source_drawer_ids: Vec<String>,
    pub card: KnowledgeCard,
    pub used_llm: bool,
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone)]
struct DrawerSignals {
    details: DrawerDetails,
    signals: AaakSignals,
}

#[derive(Debug, Clone)]
struct CardDraft {
    statement: String,
    content: String,
}

struct DeterministicDraftInput<'a> {
    primary: &'a Drawer,
    cluster_key: &'a str,
    score: f64,
    confidence: f64,
    time_span_days: f64,
    common_entities: &'a [String],
    common_topics: &'a [String],
    members: &'a [&'a DrawerSignals],
}

#[derive(Debug, Deserialize)]
struct LlmCardDraft {
    statement: String,
    content: String,
    source_drawer_ids: Vec<String>,
}

pub async fn run_crystallization(
    db: &Database,
    config: &Config,
    options: CrystallizeOptions,
) -> Result<CrystallizeSummary> {
    run_crystallization_inner(db, config, options, true).await
}

pub fn run_crystallization_deterministic(
    db: &Database,
    config: &Config,
    options: CrystallizeOptions,
) -> Result<CrystallizeSummary> {
    let runtime = tokio::runtime::Runtime::new().expect("create crystallize runtime");
    runtime.block_on(run_crystallization_inner(db, config, options, false))
}

async fn run_crystallization_inner(
    db: &Database,
    config: &Config,
    options: CrystallizeOptions,
    allow_llm: bool,
) -> Result<CrystallizeSummary> {
    if !config.crystallize.enabled {
        return Ok(CrystallizeSummary {
            candidates_found: 0,
            cards_created: 0,
            dry_run: options.dry_run,
            used_llm: false,
            fallback_count: 0,
            candidates: Vec::new(),
        });
    }

    let drawers = load_crystallize_drawers(db, options.project_id.as_deref())?;
    let mut candidates = detect_clusters(db, config, drawers)?;
    candidates.truncate(config.crystallize.max_candidates_per_run);

    let llm_client = if allow_llm && options.use_llm && config.memory_intelligence.mode.uses_llm() {
        config
            .memory_intelligence
            .has_effective_llm_endpoint(&config.llm)
            .then(|| {
                let llm_config = config.memory_intelligence.effective_llm_config(&config.llm);
                LlmClient::from_config(&llm_config)
            })
            .transpose()?
    } else {
        None
    };

    let mut cards_created = 0;
    let mut used_llm = false;
    let mut fallback_count = 0;
    for candidate in &mut candidates {
        if let Some(client) = llm_client.as_ref() {
            match enhance_candidate_with_llm(client, candidate).await {
                Ok(draft) => {
                    candidate.card.statement = draft.statement;
                    candidate.card.content = draft.content;
                    candidate.used_llm = true;
                    used_llm = true;
                    global_intelligence_status().record_success();
                }
                Err(error) => {
                    fallback_count += 1;
                    candidate.fallback_reason = Some(error.to_string());
                    global_intelligence_status().record_failure(&error);
                }
            }
        }

        if options.dry_run {
            continue;
        }
        if db.get_knowledge_card(&candidate.card.id)?.is_some() {
            continue;
        }
        insert_candidate(db, candidate)?;
        cards_created += 1;
    }

    Ok(CrystallizeSummary {
        candidates_found: candidates.len(),
        cards_created,
        dry_run: options.dry_run,
        used_llm,
        fallback_count,
        candidates,
    })
}

pub fn crystallization_score(drawer_count: usize, avg_importance: f64, time_span_days: f64) -> f64 {
    drawer_count as f64 * avg_importance * time_span_days / 30.0
}

fn load_crystallize_drawers(db: &Database, project_id: Option<&str>) -> Result<Vec<DrawerDetails>> {
    let mut statement = db.conn().prepare(
        r#"
        SELECT id
        FROM drawers
        WHERE deleted_at IS NULL
          AND compacted_into IS NULL
          AND memory_kind = 'evidence'
          AND COALESCE(is_pinned, 0) = 0
          AND ((project_id IS NULL AND ?1 IS NULL) OR project_id = ?1)
        ORDER BY id
        "#,
    )?;
    let ids = statement
        .query_map([project_id], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    db.get_drawer_details_batch(&ids).map_err(Into::into)
}

fn detect_clusters(
    db: &Database,
    config: &Config,
    drawers: Vec<DrawerDetails>,
) -> Result<Vec<CrystallizeCandidate>> {
    let drawer_signals = drawers
        .into_iter()
        .map(|details| {
            let signals = analyze(&details.drawer.content);
            (
                details.drawer.id.clone(),
                DrawerSignals { details, signals },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut groups: HashMap<String, BTreeSet<String>> = HashMap::new();
    for (drawer_id, item) in &drawer_signals {
        for key in cluster_keys(&item.signals) {
            groups.entry(key).or_default().insert(drawer_id.clone());
        }
    }

    let mut by_signature: HashMap<String, CrystallizeCandidate> = HashMap::new();
    for (cluster_key, drawer_ids) in groups {
        if drawer_ids.len() < config.crystallize.min_cluster_size {
            continue;
        }
        let members = drawer_ids
            .iter()
            .filter_map(|drawer_id| drawer_signals.get(drawer_id))
            .collect::<Vec<_>>();
        let candidate = build_candidate(db, config, cluster_key, members)?;
        if candidate.crystallization_score < config.crystallize.readiness_threshold {
            continue;
        }
        let signature = candidate.source_drawer_ids.join("\0");
        match by_signature.get(&signature) {
            Some(existing) if existing.crystallization_score >= candidate.crystallization_score => {
            }
            _ => {
                by_signature.insert(signature, candidate);
            }
        }
    }

    let mut candidates = by_signature.into_values().collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .crystallization_score
            .total_cmp(&left.crystallization_score)
            .then_with(|| right.drawer_count.cmp(&left.drawer_count))
            .then_with(|| left.cluster_key.cmp(&right.cluster_key))
    });
    Ok(candidates)
}

fn cluster_keys(signals: &AaakSignals) -> Vec<String> {
    let mut keys = Vec::new();
    for entity in signals
        .entities
        .iter()
        .filter(|entity| entity.as_str() != "UNK")
    {
        keys.push(format!("entity:{entity}"));
    }
    for topic in signals.topics.iter().take(12) {
        keys.push(format!("topic:{topic}"));
    }
    keys
}

fn build_candidate(
    db: &Database,
    config: &Config,
    cluster_key: String,
    members: Vec<&DrawerSignals>,
) -> Result<CrystallizeCandidate> {
    let source_drawer_ids = members
        .iter()
        .map(|member| member.details.drawer.id.clone())
        .collect::<Vec<_>>();
    let drawer_count = source_drawer_ids.len();
    let avg_importance = members
        .iter()
        .map(|member| member.details.drawer.importance as f64)
        .sum::<f64>()
        / drawer_count as f64;
    let avg_confidence = members
        .iter()
        .map(|member| member.details.drawer.confidence)
        .sum::<f64>()
        / drawer_count as f64;
    let time_span_days = time_span_days(&members);
    let score = crystallization_score(drawer_count, avg_importance, time_span_days);
    let common_entities = common_signal_values(&members, |signals| &signals.entities);
    let common_topics = common_signal_values(&members, |signals| &signals.topics);
    let source_files = collect_source_files(&members);
    let primary = primary_drawer(&members);
    let draft = deterministic_card_draft(DeterministicDraftInput {
        primary: &primary.details.drawer,
        cluster_key: &cluster_key,
        score,
        confidence: avg_confidence,
        time_span_days,
        common_entities: &common_entities,
        common_topics: &common_topics,
        members: &members,
    });
    let now = current_timestamp();
    let card_id = stable_card_id(&source_drawer_ids);
    let status = if config.crystallize.auto_approve {
        KnowledgeStatus::Promoted
    } else {
        KnowledgeStatus::PendingReview
    };
    let mut card = KnowledgeCard {
        id: card_id,
        statement: draft.statement,
        content: draft.content,
        tier: KnowledgeTier::Shu,
        status,
        domain: primary.details.drawer.domain,
        field: primary.details.drawer.field.clone(),
        anchor_kind: primary.details.drawer.anchor_kind.clone(),
        anchor_id: primary.details.drawer.anchor_id.clone(),
        parent_anchor_id: primary.details.drawer.parent_anchor_id.clone(),
        scope_constraints: Some(format!("auto_crystallized_from={cluster_key}")),
        trigger_hints: primary.details.drawer.trigger_hints.clone(),
        auto_generated: true,
        crystallization_score: Some(score),
        source_drawer_ids,
        created_at: now.clone(),
        updated_at: now,
    };
    if db.get_knowledge_card(&card.id)?.is_some() {
        card.scope_constraints = Some(format!("existing_auto_card={}", card.id));
    }

    Ok(CrystallizeCandidate {
        cluster_key,
        drawer_count,
        avg_importance,
        avg_confidence,
        time_span_days,
        crystallization_score: score,
        common_entities,
        common_topics,
        source_files,
        source_drawer_ids: card.source_drawer_ids.clone(),
        card,
        used_llm: false,
        fallback_reason: None,
    })
}

fn deterministic_card_draft(input: DeterministicDraftInput<'_>) -> CardDraft {
    let statement = first_statement(&input.primary.content);
    let valid_from = input
        .members
        .iter()
        .map(|member| member.details.drawer.added_at.as_str())
        .min()
        .unwrap_or(input.primary.added_at.as_str());
    let mut content = String::new();
    content.push_str("Auto-crystallized knowledge card.\n");
    content.push_str(&format!("Statement: {statement}\n"));
    content.push_str(&format!("Confidence: {:.3}\n", input.confidence));
    content.push_str(&format!("Crystallization score: {:.3}\n", input.score));
    content.push_str(&format!("Cluster key: {}\n", input.cluster_key));
    content.push_str(&format!("Valid from: {valid_from}\n"));
    content.push_str(&format!("Time span days: {:.3}\n", input.time_span_days));
    content.push_str(&format!(
        "Common entities: {}\n",
        input.common_entities.join(", ")
    ));
    content.push_str(&format!(
        "Common topics: {}\n",
        input.common_topics.join(", ")
    ));
    content.push_str("Sources:\n");
    for member in input.members {
        let drawer = &member.details.drawer;
        let source = drawer.source_file.as_deref().unwrap_or("unknown");
        content.push_str(&format!("- {} ({source})\n", drawer.id));
    }
    CardDraft { statement, content }
}

fn first_statement(content: &str) -> String {
    let trimmed = content.trim();
    let end = trimmed
        .find(['.', '\n'])
        .map(|index| index + 1)
        .unwrap_or(trimmed.len());
    let statement = trimmed[..end].trim();
    if statement.is_empty() {
        "Auto-crystallized knowledge from related evidence.".to_string()
    } else {
        statement.chars().take(240).collect()
    }
}

fn primary_drawer<'a>(members: &'a [&DrawerSignals]) -> &'a DrawerSignals {
    members
        .iter()
        .copied()
        .max_by(|left, right| {
            left.details
                .drawer
                .importance
                .cmp(&right.details.drawer.importance)
                .then_with(|| {
                    left.details
                        .drawer
                        .content
                        .len()
                        .cmp(&right.details.drawer.content.len())
                })
                .then_with(|| {
                    left.details
                        .drawer
                        .added_at
                        .cmp(&right.details.drawer.added_at)
                })
                .then_with(|| left.details.drawer.id.cmp(&right.details.drawer.id))
        })
        .expect("candidate members must not be empty")
}

fn time_span_days(members: &[&DrawerSignals]) -> f64 {
    let parsed = members
        .iter()
        .filter_map(|member| parse_temporal_timestamp_secs(&member.details.drawer.added_at))
        .collect::<Vec<_>>();
    let Some(min) = parsed.iter().min() else {
        return 0.0;
    };
    let Some(max) = parsed.iter().max() else {
        return 0.0;
    };
    ((*max - *min).max(0) as f64) / 86_400.0
}

fn common_signal_values<F>(members: &[&DrawerSignals], select: F) -> Vec<String>
where
    F: Fn(&AaakSignals) -> &[String],
{
    let mut counts: HashMap<String, usize> = HashMap::new();
    for member in members {
        let mut seen = HashSet::new();
        for value in select(&member.signals) {
            if seen.insert(value.clone()) {
                *counts.entry(value.clone()).or_default() += 1;
            }
        }
    }
    let mut values = counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .collect::<Vec<_>>();
    values.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    values
        .into_iter()
        .map(|(value, _)| value)
        .take(12)
        .collect()
}

fn collect_source_files(members: &[&DrawerSignals]) -> Vec<String> {
    members
        .iter()
        .filter_map(|member| member.details.drawer.source_file.as_deref())
        .filter(|source_file| !source_file.trim().is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn stable_card_id(source_drawer_ids: &[String]) -> String {
    let mut ids = source_drawer_ids.to_vec();
    ids.sort();
    let mut hasher = Sha256::new();
    for id in ids {
        hasher.update([0]);
        hasher.update(id.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("card_auto_{}", &digest[..16])
}

async fn enhance_candidate_with_llm(
    client: &LlmClient,
    candidate: &CrystallizeCandidate,
) -> Result<CardDraft> {
    let payload = serde_json::json!({
        "cluster_key": candidate.cluster_key,
        "source_drawer_ids": candidate.source_drawer_ids,
        "deterministic_statement": candidate.card.statement,
        "deterministic_content": candidate.card.content,
    });
    let response = client
        .chat_completion(&LlmRequest {
            messages: vec![
                LlmMessage {
                    role: "system".to_string(),
                    content: "Return strict JSON with keys statement, content, source_drawer_ids. Preserve exactly all source_drawer_ids from the input and do not add facts unsupported by the cited drawer ids.".to_string(),
                },
                LlmMessage {
                    role: "user".to_string(),
                    content: payload.to_string(),
                },
            ],
            model: None,
            temperature: Some(0.0),
            max_tokens: Some(1024),
        })
        .await?;
    let decoded: LlmCardDraft = serde_json::from_str(response.content.trim())?;
    validate_llm_draft(candidate, decoded)
}

fn validate_llm_draft(candidate: &CrystallizeCandidate, draft: LlmCardDraft) -> Result<CardDraft> {
    if draft.statement.trim().is_empty() || draft.content.trim().is_empty() {
        return Err(CrystallizeError::InvalidLlmDraft(
            "statement/content must not be empty".to_string(),
        ));
    }
    let expected = candidate
        .source_drawer_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let actual = draft
        .source_drawer_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(CrystallizeError::InvalidLlmDraft(
            "source_drawer_ids were not preserved".to_string(),
        ));
    }
    Ok(CardDraft {
        statement: draft.statement.trim().to_string(),
        content: draft.content.trim().to_string(),
    })
}

fn insert_candidate(db: &Database, candidate: &CrystallizeCandidate) -> Result<()> {
    db.insert_knowledge_card(&candidate.card)?;
    for drawer_id in &candidate.source_drawer_ids {
        db.insert_knowledge_evidence_link(&KnowledgeEvidenceLink {
            id: stable_link_id(&candidate.card.id, drawer_id),
            card_id: candidate.card.id.clone(),
            evidence_drawer_id: drawer_id.clone(),
            role: KnowledgeEvidenceRole::Supporting,
            note: Some("auto-crystallize source".to_string()),
            created_at: candidate.card.created_at.clone(),
        })?;
    }
    db.append_knowledge_event(&KnowledgeCardEvent {
        id: stable_event_id(&candidate.card.id, &candidate.card.created_at),
        card_id: candidate.card.id.clone(),
        event_type: KnowledgeEventType::Created,
        from_status: None,
        to_status: Some(candidate.card.status.clone()),
        reason: "auto-crystallize generated this card from repeated evidence".to_string(),
        actor: Some("mempal.crystallize".to_string()),
        metadata: Some(serde_json::json!({
            "auto_generated": true,
            "cluster_key": candidate.cluster_key,
            "crystallization_score": candidate.crystallization_score,
            "source_drawer_ids": candidate.source_drawer_ids,
        })),
        created_at: candidate.card.created_at.clone(),
    })?;
    Ok(())
}

fn stable_link_id(card_id: &str, drawer_id: &str) -> String {
    stable_prefixed_id("link_auto", &[card_id, drawer_id])
}

fn stable_event_id(card_id: &str, created_at: &str) -> String {
    stable_prefixed_id("event_auto", &[card_id, created_at])
}

fn stable_prefixed_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    let digest = format!("{:x}", hasher.finalize());
    format!("{prefix}_{}", &digest[..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::config::Config;
    use crate::core::types::{BootstrapEvidenceArgs, SourceType};
    use tempfile::TempDir;

    fn new_db() -> (TempDir, Database) {
        let tmp = TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        (tmp, db)
    }

    fn drawer(id: &str, day: u32, importance: i32, content: &str) -> Drawer {
        Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: "mempal".to_string(),
            room: Some("crystallize".to_string()),
            source_file: Some(format!("tests://{id}")),
            source_type: SourceType::AgentInference,
            added_at: format!("2026-01-{day:02}T00:00:00Z"),
            chunk_index: Some(0),
            importance,
        })
    }

    fn seed_related_drawers(db: &Database) {
        for day in 1..=5 {
            let drawer = drawer(
                &format!("drawer_{day}"),
                day,
                5,
                "Decision: Rust memory cards should preserve citations for Mempal crystallization.",
            );
            db.insert_drawer(&drawer).expect("insert drawer");
        }
    }

    fn config() -> Config {
        let mut config = Config::default();
        config.crystallize.min_cluster_size = 5;
        config.crystallize.readiness_threshold = 0.1;
        config.crystallize.max_candidates_per_run = 20;
        config
    }

    #[test]
    fn test_cluster_detection_finds_related_drawers() {
        let (_tmp, db) = new_db();
        seed_related_drawers(&db);

        let summary = run_crystallization_deterministic(
            &db,
            &config(),
            CrystallizeOptions {
                dry_run: true,
                project_id: None,
                use_llm: false,
            },
        )
        .expect("crystallize");

        assert_eq!(summary.candidates_found, 1);
        assert_eq!(summary.candidates[0].drawer_count, 5);
    }

    #[test]
    fn test_crystallization_score_formula() {
        assert_eq!(crystallization_score(5, 3.0, 30.0), 15.0);
    }

    #[test]
    fn test_auto_card_includes_citations() {
        let (_tmp, db) = new_db();
        seed_related_drawers(&db);

        let summary = run_crystallization_deterministic(
            &db,
            &config(),
            CrystallizeOptions {
                dry_run: false,
                project_id: None,
                use_llm: false,
            },
        )
        .expect("crystallize");
        let card = db
            .get_knowledge_card(&summary.candidates[0].card.id)
            .expect("get card")
            .expect("card exists");

        assert_eq!(card.source_drawer_ids.len(), 5);
        for drawer_id in &card.source_drawer_ids {
            assert!(card.content.contains(drawer_id));
        }
        assert!(card.crystallization_score.is_some());
    }

    #[test]
    fn test_pending_review_gate() {
        let (_tmp, db) = new_db();
        seed_related_drawers(&db);

        let summary = run_crystallization_deterministic(
            &db,
            &config(),
            CrystallizeOptions {
                dry_run: false,
                project_id: None,
                use_llm: false,
            },
        )
        .expect("crystallize");

        assert_eq!(
            summary.candidates[0].card.status,
            KnowledgeStatus::PendingReview
        );
        assert_eq!(
            db.pending_auto_generated_knowledge_card_count()
                .expect("pending count"),
            1
        );
    }

    #[test]
    fn test_auto_approve_config() {
        let (_tmp, db) = new_db();
        seed_related_drawers(&db);
        let mut config = config();
        config.crystallize.auto_approve = true;

        let summary = run_crystallization_deterministic(
            &db,
            &config,
            CrystallizeOptions {
                dry_run: false,
                project_id: None,
                use_llm: false,
            },
        )
        .expect("crystallize");

        assert_eq!(summary.candidates[0].card.status, KnowledgeStatus::Promoted);
    }

    #[test]
    fn test_dry_run_no_cards_created() {
        let (_tmp, db) = new_db();
        seed_related_drawers(&db);

        let summary = run_crystallization_deterministic(
            &db,
            &config(),
            CrystallizeOptions {
                dry_run: true,
                project_id: None,
                use_llm: false,
            },
        )
        .expect("crystallize");

        assert_eq!(summary.cards_created, 0);
        assert_eq!(db.knowledge_card_count().expect("card count"), 0);
    }
}
