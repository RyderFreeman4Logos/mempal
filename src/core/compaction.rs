use std::collections::{BTreeSet, HashSet};

use super::{
    db::{Database, DbError},
    types::{CompactionResult, CompactionStrategy, Drawer, DrawerDetails},
};

pub fn select_richest(drawers: &[Drawer]) -> &Drawer {
    drawers
        .iter()
        .max_by(|left, right| {
            left.importance
                .cmp(&right.importance)
                .then_with(|| left.content.len().cmp(&right.content.len()))
                .then_with(|| left.added_at.cmp(&right.added_at))
                .then_with(|| left.id.cmp(&right.id))
        })
        .expect("select_richest requires at least one drawer")
}

pub fn merge_cluster(
    db: &Database,
    cluster_drawer_ids: &[String],
    strategy: CompactionStrategy,
    dry_run: bool,
) -> Result<CompactionResult, DbError> {
    match strategy {
        CompactionStrategy::RichestContent => {}
        CompactionStrategy::LlmSummary => return Err(DbError::LlmCompactionNotImplemented),
    }

    let source_ids = dedupe_preserving_order(cluster_drawer_ids);
    if source_ids.is_empty() {
        return Err(DbError::CompactionClusterEmpty);
    }

    let details = db.get_drawer_details_batch(&source_ids)?;
    if details.len() != source_ids.len() {
        let found = details
            .iter()
            .map(|details| details.drawer.id.as_str())
            .collect::<HashSet<_>>();
        let missing = source_ids
            .iter()
            .find(|drawer_id| !found.contains(drawer_id.as_str()))
            .cloned()
            .unwrap_or_else(|| source_ids[0].clone());
        return Err(DbError::CompactionDrawerNotFound { drawer_id: missing });
    }

    let drawers = details
        .iter()
        .map(|details| details.drawer.clone())
        .collect::<Vec<_>>();
    let target_id = select_richest(&drawers).id.clone();
    let target = details
        .iter()
        .find(|details| details.drawer.id == target_id)
        .expect("selected target must come from loaded drawer details");
    let citations = collect_source_citations(&details);
    let merged_content = append_consolidated_citations(&target.drawer.content, &citations);

    if !dry_run {
        db.apply_compaction(&target_id, &source_ids, &merged_content, strategy)?;
    }

    Ok(CompactionResult {
        target_id,
        source_ids,
        strategy,
        cluster_size: drawers.len(),
        dry_run,
    })
}

fn dedupe_preserving_order(drawer_ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    drawer_ids
        .iter()
        .filter(|drawer_id| seen.insert((*drawer_id).clone()))
        .cloned()
        .collect()
}

fn collect_source_citations(details: &[DrawerDetails]) -> Vec<String> {
    details
        .iter()
        .filter_map(|details| details.drawer.source_file.as_deref())
        .filter(|source_file| !source_file.trim().is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn append_consolidated_citations(content: &str, citations: &[String]) -> String {
    if citations.is_empty() {
        return content.to_string();
    }

    let mut merged = content.trim_end().to_string();
    merged.push_str("\n\n---\nConsolidated citations:\n");
    for citation in citations {
        merged.push_str("- ");
        merged.push_str(citation);
        merged.push('\n');
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{BootstrapEvidenceArgs, SourceType};

    fn drawer(id: &str, content: &str, importance: i32, added_at: &str) -> Drawer {
        Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: id.to_string(),
            content: content.to_string(),
            wing: "memory".to_string(),
            room: Some("decision".to_string()),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::AgentInference,
            added_at: added_at.to_string(),
            chunk_index: Some(0),
            importance,
        })
    }

    fn db_with_drawers(drawers: &[Drawer]) -> (tempfile::TempDir, Database) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        for drawer in drawers {
            db.insert_drawer_with_project(drawer, Some("project-a"))
                .expect("insert drawer");
            db.insert_vector_with_project(&drawer.id, &[1.0, 0.0, 0.0], Some("project-a"))
                .expect("insert vector");
        }
        (tempdir, db)
    }

    #[test]
    fn test_select_richest_prefers_importance_length_then_recency() {
        let low_importance = drawer("low", "longer content", 2, "2026-05-13T00:00:00Z");
        let shorter = drawer("shorter", "short", 5, "2026-05-13T00:00:00Z");
        let older = drawer("older", "same length", 5, "2026-05-12T00:00:00Z");
        let newest = drawer("newest", "same length", 5, "2026-05-14T00:00:00Z");

        assert_eq!(
            select_richest(&[low_importance, shorter, older, newest]).id,
            "newest"
        );
    }

    #[test]
    fn test_merge_cluster_dry_run_returns_preview_without_mutations() {
        let drawers = vec![
            drawer("a", "alpha", 1, "2026-05-13T00:00:00Z"),
            drawer("b", "beta beta", 3, "2026-05-13T00:00:00Z"),
            drawer("c", "gamma", 2, "2026-05-13T00:00:00Z"),
        ];
        let (_tmp, db) = db_with_drawers(&drawers);
        let ids = drawers
            .iter()
            .map(|drawer| drawer.id.clone())
            .collect::<Vec<_>>();

        let result = merge_cluster(&db, &ids, CompactionStrategy::RichestContent, true)
            .expect("dry-run merge");

        assert_eq!(result.target_id, "b");
        assert_eq!(result.cluster_size, 3);
        assert!(result.dry_run);
        assert_eq!(db.drawer_count().expect("drawer count"), 3);
        let log_count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM consolidation_log", [], |row| {
                row.get(0)
            })
            .expect("log count");
        assert_eq!(log_count, 0);
    }

    #[test]
    fn test_merge_cluster_soft_deletes_sources_and_records_log() {
        let drawers = vec![
            drawer("target", "rich content wins", 5, "2026-05-13T00:00:00Z"),
            drawer("source-a", "short", 1, "2026-05-13T00:00:00Z"),
            drawer("source-b", "also short", 1, "2026-05-13T00:00:00Z"),
        ];
        let (_tmp, db) = db_with_drawers(&drawers);
        let ids = drawers
            .iter()
            .map(|drawer| drawer.id.clone())
            .collect::<Vec<_>>();

        let result = merge_cluster(&db, &ids, CompactionStrategy::RichestContent, false)
            .expect("merge cluster");

        assert_eq!(result.target_id, "target");
        let target = db
            .get_drawer("target")
            .expect("load target")
            .expect("target remains active");
        assert!(target.content.starts_with("rich content wins"));
        assert!(target.content.contains("Consolidated citations:"));
        assert!(db.get_drawer("source-a").expect("load source").is_none());

        let source_a = db
            .conn()
            .query_row(
                "SELECT deleted_at IS NOT NULL, compacted_into FROM drawers WHERE id = 'source-a'",
                [],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .expect("read source compaction state");
        assert_eq!(source_a, (true, Some("target".to_string())));

        let log = db
            .conn()
            .query_row(
                r#"
                SELECT cluster_size, strategy, target_drawer_id, source_drawer_ids, dry_run
                FROM consolidation_log
                "#,
                [],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .expect("read consolidation log");
        assert_eq!(log.0, 3);
        assert_eq!(log.1, "richest_content");
        assert_eq!(log.2, "target");
        assert_eq!(log.4, 0);
        let source_ids: Vec<String> = serde_json::from_str(&log.3).expect("source ids json");
        assert_eq!(source_ids, ids);
    }

    #[test]
    fn test_llm_summary_strategy_is_stubbed() {
        let drawers = vec![
            drawer("a", "alpha", 1, "2026-05-13T00:00:00Z"),
            drawer("b", "beta", 1, "2026-05-13T00:00:00Z"),
        ];
        let (_tmp, db) = db_with_drawers(&drawers);
        let ids = drawers
            .iter()
            .map(|drawer| drawer.id.clone())
            .collect::<Vec<_>>();

        let error =
            merge_cluster(&db, &ids, CompactionStrategy::LlmSummary, true).expect_err("stub error");

        assert!(matches!(error, DbError::LlmCompactionNotImplemented));
        assert_eq!(error.to_string(), "LLM compaction not yet implemented");
    }
}
