use std::path::PathBuf;

use mempal::brief::{BriefRequest, assemble_brief_from_bm25};
use mempal::core::anchor;
use mempal::core::db::Database;
use mempal::core::types::{AnchorKind, Drawer, MemoryDomain, MemoryKind, Provenance, SourceType};
use tempfile::TempDir;

fn evidence(id: &str, content: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "mempal".to_string(),
        room: Some("brief".to_string()),
        source_file: Some(format!("tests://brief/{id}")),
        source_type: SourceType::UserExplicit,
        added_at: "1710000000".to_string(),
        chunk_index: Some(0),
        normalize_version: 1,
        importance: 3,
        memory_kind: MemoryKind::Evidence,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        parent_anchor_id: None,
        provenance: Some(Provenance::Human),
        statement: None,
        tier: None,
        status: None,
        supporting_refs: Vec::new(),
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: None,
        trigger_hints: None,
        is_pinned: false,
        pin_order: None,
        supersedes: None,
        effective_importance: 3.0,
        compacted_into: None,
        confidence: 1.0,
    }
}

#[test]
fn unscoped_bm25_brief_keeps_project_tagged_drawer() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    db.insert_drawer_with_project(
        &evidence(
            "tagged-drawer",
            "Unscoped BM25 brief must still cite this project-tagged drawer.",
        ),
        Some("alpha-app"),
    )
    .expect("insert project-tagged drawer");

    let brief = assemble_brief_from_bm25(
        &db,
        BriefRequest {
            query: "project-tagged drawer".to_string(),
            domain: MemoryDomain::Project,
            field: "general".to_string(),
            cwd: PathBuf::from("/tmp"),
            max_items: 8,
            dao_tian_limit: 4,
        },
        "bm25-only".to_string(),
    )
    .expect("assemble unscoped BM25 brief");

    assert!(
        brief
            .evidence
            .iter()
            .any(|item| item.citation.drawer_id == "tagged-drawer"),
        "unscoped BM25 brief dropped the project-tagged drawer: {brief:?}"
    );
}
