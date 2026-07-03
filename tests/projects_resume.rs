// P109 acceptance: cross-project list + fuzzy resume against the global DB.

use mempal::core::db::Database;
use mempal::core::types::{
    AnchorKind, Drawer, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, Provenance,
    SourceType,
};
use mempal::projects::{ResumeResolution, list_projects, resume_project};
use tempfile::TempDir;

fn new_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    (tmp, db)
}

fn worktree_evidence(id: &str, wing: &str, abs_path: &str, added_at: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: format!("decision recorded for {wing}"),
        wing: wing.to_string(),
        room: Some("work".to_string()),
        source_file: Some(format!("tests://{wing}/{id}")),
        source_type: SourceType::UserExplicit,
        added_at: added_at.to_string(),
        chunk_index: Some(0),
        importance: 3,
        memory_kind: MemoryKind::Evidence,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Worktree,
        anchor_id: format!("worktree://{abs_path}"),
        parent_anchor_id: None,
        provenance: Some(Provenance::Human),
        effective_importance: 3.0,
        ..Drawer::default()
    }
}

fn candidate_knowledge(id: &str, wing: &str, statement: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: "candidate knowledge body".to_string(),
        wing: wing.to_string(),
        room: Some("work".to_string()),
        source_file: Some(format!("knowledge://{wing}/{id}")),
        source_type: SourceType::UserExplicit,
        added_at: "1710000500".to_string(),
        chunk_index: Some(0),
        importance: 3,
        memory_kind: MemoryKind::Knowledge,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Worktree,
        anchor_id: format!("worktree:///Work/{wing}"),
        parent_anchor_id: None,
        statement: Some(statement.to_string()),
        tier: Some(KnowledgeTier::DaoRen),
        status: Some(KnowledgeStatus::Candidate),
        supporting_refs: vec!["drawer_x".to_string()],
        effective_importance: 3.0,
        ..Drawer::default()
    }
}

#[test]
fn test_list_projects_reports_paths_counts_and_newest_first() {
    let (_tmp, db) = new_db();
    db.insert_drawer(&worktree_evidence(
        "d_alpha_1",
        "alpha",
        "/Work/alpha-old",
        "1710000001",
    ))
    .unwrap();
    db.insert_drawer(&worktree_evidence(
        "d_alpha_2",
        "alpha",
        "/Work/alpha",
        "1710000003",
    ))
    .unwrap();
    db.insert_drawer(&worktree_evidence(
        "d_beta_1",
        "beta",
        "/Work/beta",
        "1710000002",
    ))
    .unwrap();

    let projects = list_projects(&db).expect("list projects");
    assert_eq!(projects[0].wing, "alpha");
    assert_eq!(projects[1].wing, "beta");

    let alpha = projects.iter().find(|p| p.wing == "alpha").expect("alpha");
    let beta = projects.iter().find(|p| p.wing == "beta").expect("beta");
    assert_eq!(alpha.path.as_deref(), Some("/Work/alpha"));
    assert_eq!(beta.path.as_deref(), Some("/Work/beta"));
    assert_eq!(alpha.total, 2);
    assert_eq!(alpha.evidence, 2);
    assert_eq!(alpha.knowledge, 0);
}

#[test]
fn test_resume_resolves_unique_match() {
    let (_tmp, db) = new_db();
    db.insert_drawer(&worktree_evidence(
        "d_auth_1",
        "auth-service",
        "/Work/auth-service",
        "1710000010",
    ))
    .unwrap();
    db.insert_drawer(&candidate_knowledge(
        "k_auth_1",
        "auth-service",
        "auth uses JWT",
    ))
    .unwrap();

    let resolution = resume_project(&db, "auth", 5, 5).expect("resume");
    match resolution {
        ResumeResolution::Resolved(pack) => {
            assert_eq!(pack.wing, "auth-service");
            assert_eq!(pack.path.as_deref(), Some("/Work/auth-service"));
            assert_eq!(pack.total, 2);
            assert_eq!(pack.evidence, 1);
            assert_eq!(pack.knowledge, 1);
            assert_eq!(pack.recent_evidence.len(), 1);
            assert_eq!(pack.in_flight.len(), 1);
            assert!(pack.next_step.contains("/Work/auth-service"));
        }
        other => panic!("expected resolved, got {other:?}"),
    }
}

#[test]
fn test_resume_matches_worktree_basename() {
    let (_tmp, db) = new_db();
    db.insert_drawer(&worktree_evidence(
        "d_path_1",
        "internal-wing",
        "/Work/friendly-project",
        "1710000010",
    ))
    .unwrap();

    let resolution = resume_project(&db, "friendly", 5, 5).expect("resume");
    match resolution {
        ResumeResolution::Resolved(pack) => {
            assert_eq!(pack.wing, "internal-wing");
            assert_eq!(pack.path.as_deref(), Some("/Work/friendly-project"));
        }
        other => panic!("expected resolved, got {other:?}"),
    }
}

#[test]
fn test_resume_reports_ambiguous_matches() {
    let (_tmp, db) = new_db();
    db.insert_drawer(&worktree_evidence(
        "d_a1",
        "agentchat-alpha",
        "/Work/agentchat-alpha",
        "1710000020",
    ))
    .unwrap();
    db.insert_drawer(&worktree_evidence(
        "d_b1",
        "agentchat-beta",
        "/Work/agentchat-beta",
        "1710000021",
    ))
    .unwrap();

    let resolution = resume_project(&db, "agentchat", 5, 5).expect("resume");
    match resolution {
        ResumeResolution::Ambiguous { candidates, .. } => {
            assert_eq!(candidates.len(), 2);
        }
        other => panic!("expected ambiguous, got {other:?}"),
    }
}

#[test]
fn test_resume_reports_not_found() {
    let (_tmp, db) = new_db();
    db.insert_drawer(&worktree_evidence(
        "d_only",
        "solo",
        "/Work/solo",
        "1710000030",
    ))
    .unwrap();

    let resolution = resume_project(&db, "nonexistent", 5, 5).expect("resume");
    match resolution {
        ResumeResolution::NotFound { available, .. } => {
            assert!(available.contains(&"solo".to_string()));
        }
        other => panic!("expected not found, got {other:?}"),
    }
}
