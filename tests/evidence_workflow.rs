#![cfg(feature = "adk-rust")]

use std::sync::Arc;

use async_trait::async_trait;
use mempal::core::config::{Config, ConfigHandle, EvidenceWorkflowConfig};
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder, EmbedderFactory};
use mempal::evidence_workflow::{
    CitedHit, EvidenceFallbackReason, EvidenceRoute, EvidenceScoreType, run_evidence_workflow,
};
use mempal::mcp::{MempalMcpServer, SearchRequest};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

#[derive(Clone)]
struct StaticEmbedderFactory;

struct StaticEmbedder;

#[async_trait]
impl EmbedderFactory for StaticEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(StaticEmbedder))
    }
}

#[async_trait]
impl Embedder for StaticEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "static-evidence-test"
    }
}

#[tokio::test]
async fn quality_gate_preserves_exact_citations_end_to_end() {
    let config = EvidenceWorkflowConfig {
        enabled: true,
        minimum_relevance: 0.75,
        output_top_k: 2,
        ..EvidenceWorkflowConfig::default()
    };
    let exact_quote = "ADK-Rust orchestrates post-retrieval evidence selection.";
    let strong = CitedHit::new(
        "drawer-strong",
        "file:///memory/session-42.jsonl",
        "project",
        "agent_inference",
        exact_quote,
        EvidenceScoreType::Vector,
        0.93,
    );
    let weak = CitedHit::new(
        "drawer-weak",
        "file:///memory/session-7.jsonl",
        "project",
        "agent_inference",
        "This candidate is below the configured quality floor.",
        EvidenceScoreType::Vector,
        0.31,
    );

    let pack = run_evidence_workflow(&config, vec![strong.clone(), weak]).await;

    assert_eq!(pack.route, EvidenceRoute::QualityGatedEvidence);
    assert_eq!(pack.items.len(), 1);
    let item = &pack.items[0];
    assert_eq!(item.hit_id, strong.hit_id);
    assert_eq!(item.source_uri, strong.source_uri);
    assert_eq!(item.source_kind, strong.source_kind);
    assert_eq!(item.content_hash, strong.content_hash);
    assert_eq!(item.exact_quote, exact_quote);
    assert_eq!(item.source_span, strong.source_span);
    assert_eq!(item.score_type, strong.score_type);
    assert_eq!(item.relevance_score, strong.relevance_score);
    assert_eq!(pack.metrics.retrieved_hits, 2);
    assert_eq!(pack.metrics.selected_hits, 1);
    assert!(pack.fallback_reason.is_none());
}

#[tokio::test]
async fn workflow_is_disabled_by_default_and_preserves_raw_cited_hits() {
    let config = EvidenceWorkflowConfig::default();
    assert!(!config.enabled);
    let raw = CitedHit::new(
        "drawer-default-off",
        "file:///memory/default-off.jsonl",
        "global",
        "user_explicit",
        "Default configuration must not transform this retrieved hit.",
        EvidenceScoreType::Vector,
        0.99,
    );

    let pack = run_evidence_workflow(&config, vec![raw.clone()]).await;

    assert_eq!(pack.route, EvidenceRoute::RawBoundedHits);
    assert_eq!(pack.fallback_reason, Some(EvidenceFallbackReason::Disabled));
    assert_eq!(pack.items.len(), 1);
    assert_eq!(pack.items[0].hit_id, raw.hit_id);
    assert_eq!(pack.items[0].exact_quote, raw.exact_quote);
    assert_eq!(pack.items[0].content_hash, raw.content_hash);
}

#[tokio::test]
async fn empty_quality_selection_routes_to_bounded_cited_fallback() {
    let config = EvidenceWorkflowConfig {
        enabled: true,
        minimum_relevance: 0.9,
        ..EvidenceWorkflowConfig::default()
    };
    let weak = CitedHit::new(
        "drawer-below-floor",
        "file:///memory/below-floor.jsonl",
        "project",
        "agent_inference",
        "This hit remains cited when the quality gate rejects transformation.",
        EvidenceScoreType::Vector,
        0.4,
    );

    let pack = run_evidence_workflow(&config, vec![weak.clone()]).await;

    assert_eq!(pack.route, EvidenceRoute::RawBoundedHits);
    assert_eq!(
        pack.fallback_reason,
        Some(EvidenceFallbackReason::BelowQualityThreshold)
    );
    assert_eq!(pack.items[0].hit_id, weak.hit_id);
    assert_eq!(pack.items[0].source_uri, weak.source_uri);
    assert_eq!(pack.items[0].exact_quote, weak.exact_quote);
}

#[tokio::test]
async fn mcp_search_default_quality_threshold_accepts_hybrid_rrf_score() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config_path = tmp.path().join("config.toml");
    let mut config = Config {
        db_path: db_path.display().to_string(),
        ..Config::default()
    };
    config.evidence_workflow.enabled = true;
    config.save_to(&config_path).expect("save config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");

    let content = "The production MCP caller preserves this exact evidence quote.";
    let source_file = "/memory/project/session-42.jsonl";
    let db = Database::open(&db_path).expect("open database");
    db.insert_drawer(&Drawer {
        id: "drawer-mcp-evidence".to_string(),
        content: content.to_string(),
        wing: "code".to_string(),
        room: Some("evidence".to_string()),
        source_file: Some(source_file.to_string()),
        source_type: SourceType::AgentInference,
        added_at: "1713000000".to_string(),
        importance: 4,
        ..Drawer::default()
    })
    .expect("insert drawer");
    db.insert_vector("drawer-mcp-evidence", &[1.0, 0.0, 0.0])
        .expect("insert vector");

    let server = MempalMcpServer::new_with_factory_and_config(
        db_path,
        config,
        Arc::new(StaticEmbedderFactory),
    )
    .expect("create MCP server");
    let response = server
        .mempal_search(Parameters(SearchRequest {
            query: "production MCP evidence quote".to_string(),
            top_k: Some(10),
            evidence: Some(true),
            disable_progressive: Some(true),
            ..SearchRequest::default()
        }))
        .await
        .expect("MCP search")
        .0;

    let pack = response.evidence.expect("evidence pack");
    assert_eq!(pack.route, EvidenceRoute::QualityGatedEvidence);
    assert_eq!(pack.items.len(), 1);
    assert_eq!(pack.items[0].hit_id, "drawer-mcp-evidence");
    assert_eq!(pack.items[0].source_uri, source_file);
    assert_eq!(pack.items[0].exact_quote, content);
    assert_eq!(pack.items[0].score_type, EvidenceScoreType::Fused);
    assert!(pack.items[0].relevance_score < 0.65);
}
