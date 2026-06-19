#![warn(clippy::all)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Barrier};
use std::thread;

use mempal::core::{
    case_skill::{
        CaseCloseRequest, CaseOpenRequest, CaseSkillError, CaseVerdict, SkillProposalOptions,
        close_case, open_case, propose_skills_from_cases,
    },
    db::Database,
    skills::{SkillStatus, adopt_skill, list_skills, load_active_skills_for_context},
    types::{BootstrapEvidenceArgs, Drawer, KnowledgeStatus, MemoryKind, SourceType},
};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn new_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    (tmp, db)
}

fn insert_evidence(db: &Database, id: &str, content: &str) {
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: content.to_string(),
        wing: "mempal".to_string(),
        room: Some("evidence".to_string()),
        source_file: Some(format!("tests://{id}")),
        source_type: SourceType::AgentObservation,
        added_at: "2026-06-19T00:00:00Z".to_string(),
        chunk_index: Some(0),
        importance: 3,
    });
    db.insert_drawer(&drawer).expect("insert evidence");
}

fn open_test_case(db: &Database, procedure_key: &str, task: &str) -> String {
    open_case(
        db,
        CaseOpenRequest {
            task: task.to_string(),
            procedure_key: procedure_key.to_string(),
            procedure_summary: "Run focused verification before promotion".to_string(),
            procedure_steps: vec![
                "inspect typed drawer rows".to_string(),
                "run focused tests".to_string(),
            ],
            trajectory: vec!["implemented deterministic path".to_string()],
            anti_patterns: Vec::new(),
            failed_approaches: Vec::new(),
            wing: "mempal".to_string(),
            room: "cases".to_string(),
            project_id: None,
            importance: 3,
            dry_run: false,
        },
    )
    .expect("open case")
    .case_id
}

fn close_success(db: &Database, case_id: &str, verification_ref: &str) {
    close_case(
        db,
        CaseCloseRequest {
            case_id: case_id.to_string(),
            verdict: CaseVerdict::Success,
            tests: vec!["cargo test --test case_skill".to_string()],
            verification_refs: vec![verification_ref.to_string()],
            anti_patterns: Vec::new(),
            failed_approaches: Vec::new(),
        },
    )
    .expect("close success");
}

fn run_mempal(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .env("HOME", home)
        .args(args)
        .output()
        .expect("run mempal")
}

fn parse_json_field(stdout: &[u8], field: &str) -> String {
    let value: serde_json::Value = serde_json::from_slice(stdout).expect("json stdout");
    value[field].as_str().expect("string field").to_string()
}

fn drawer_project_id(db: &Database, drawer_id: &str) -> Option<String> {
    db.conn()
        .query_row(
            "SELECT project_id FROM drawers WHERE id = ?1",
            [drawer_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .expect("query drawer project")
}

#[test]
fn test_second_connection_cannot_overwrite_closed_case() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db_a = Database::open(&db_path).expect("open db a");
    let db_b = Database::open(&db_path).expect("open db b");
    insert_evidence(&db_a, "drawer_verify_first", "first close proof");
    insert_evidence(&db_a, "drawer_verify_second", "second close proof");

    let case_id = open_test_case(&db_a, "case-skill.concurrent-close", "close once");
    close_case(
        &db_a,
        CaseCloseRequest {
            case_id: case_id.clone(),
            verdict: CaseVerdict::Success,
            tests: vec!["first close sentinelalpha".to_string()],
            verification_refs: vec!["drawer_verify_first".to_string()],
            anti_patterns: Vec::new(),
            failed_approaches: Vec::new(),
        },
    )
    .expect("first close succeeds");

    let second = close_case(
        &db_b,
        CaseCloseRequest {
            case_id: case_id.clone(),
            verdict: CaseVerdict::Success,
            tests: vec!["second close sentinelbeta".to_string()],
            verification_refs: vec!["drawer_verify_second".to_string()],
            anti_patterns: Vec::new(),
            failed_approaches: Vec::new(),
        },
    );
    assert!(
        matches!(second, Err(CaseSkillError::CaseAlreadyClosed(ref id)) if id == &case_id),
        "second close should report already closed, got {second:?}"
    );

    let closed = db_a
        .get_drawer(&case_id)
        .expect("load closed case")
        .expect("closed case exists");
    assert_eq!(closed.verification_refs, vec!["drawer_verify_first"]);
    let content: mempal::core::case_skill::CaseContent =
        serde_json::from_str(&closed.content).expect("case json");
    assert_eq!(content.verdict, CaseVerdict::Success);
    assert_eq!(content.tests, vec!["first close sentinelalpha"]);

    let overwritten_ids: Vec<String> = db_a
        .search_fts(
            "sentinelbeta",
            Some("mempal"),
            Some("cases"),
            "all",
            None,
            10,
        )
        .expect("search overwritten sentinel")
        .into_iter()
        .map(|(id, _rank)| id)
        .collect();
    assert!(
        !overwritten_ids.iter().any(|id| id == &case_id),
        "failed second close must not update FTS with stale content"
    );
}

#[test]
fn test_case_lifecycle_stores_verdict_tests_and_verification_refs() {
    let (_tmp, db) = new_db();
    insert_evidence(&db, "drawer_verify_case", "focused test output");

    let case_id = open_test_case(&db, "case-skill.lifecycle", "implement case lifecycle");
    let open_drawer = db
        .get_drawer(&case_id)
        .expect("load open case")
        .expect("open case exists");
    assert_eq!(open_drawer.memory_kind, MemoryKind::Case);
    assert_eq!(open_drawer.status, Some(KnowledgeStatus::PendingReview));

    close_case(
        &db,
        CaseCloseRequest {
            case_id: case_id.clone(),
            verdict: CaseVerdict::Success,
            tests: vec!["git diff --check passed".to_string()],
            verification_refs: vec!["drawer_verify_case".to_string()],
            anti_patterns: Vec::new(),
            failed_approaches: Vec::new(),
        },
    )
    .expect("close case");

    let closed = db
        .get_drawer(&case_id)
        .expect("load closed case")
        .expect("closed case exists");
    assert_eq!(closed.status, Some(KnowledgeStatus::Active));
    assert_eq!(closed.verification_refs, vec!["drawer_verify_case"]);
    let content: mempal::core::case_skill::CaseContent =
        serde_json::from_str(&closed.content).expect("case json");
    assert_eq!(content.verdict, CaseVerdict::Success);
    assert_eq!(content.tests, vec!["git diff --check passed"]);
}

#[test]
fn test_skill_propose_from_cases_groups_threshold_and_preserves_failures() {
    let (_tmp, db) = new_db();
    insert_evidence(&db, "drawer_verify_a", "test A passed");
    insert_evidence(&db, "drawer_verify_b", "test B passed");

    let case_a = open_test_case(&db, "case-skill.repeatable", "first success");
    let case_b = open_test_case(&db, "case-skill.repeatable", "second success");
    let case_failed = open_test_case(&db, "case-skill.repeatable", "failed attempt");
    close_success(&db, &case_a, "drawer_verify_a");
    close_success(&db, &case_b, "drawer_verify_b");
    close_case(
        &db,
        CaseCloseRequest {
            case_id: case_failed.clone(),
            verdict: CaseVerdict::Failure,
            tests: vec!["cargo test failed before fixture setup".to_string()],
            verification_refs: Vec::new(),
            anti_patterns: vec!["do not infer repeated procedure without explicit key".to_string()],
            failed_approaches: vec!["fuzzy grouping by prose summary".to_string()],
        },
    )
    .expect("close failure");

    let below_threshold = propose_skills_from_cases(
        &db,
        SkillProposalOptions {
            from_cases: true,
            min_support: 3,
            min_verification_refs: 1,
            wing: Some("mempal".to_string()),
            project_id: None,
            dry_run: false,
        },
    )
    .expect("propose below threshold");
    assert!(below_threshold.proposals.is_empty());

    let proposed = propose_skills_from_cases(
        &db,
        SkillProposalOptions {
            from_cases: true,
            min_support: 2,
            min_verification_refs: 1,
            wing: Some("mempal".to_string()),
            project_id: None,
            dry_run: false,
        },
    )
    .expect("propose skills");
    assert_eq!(proposed.proposals.len(), 1);

    let proposal = &proposed.proposals[0];
    assert_eq!(proposal.support_count, 2);
    assert_eq!(proposal.verification_ref_count, 2);
    assert_eq!(proposal.counterexample_count, 1);
    assert!(proposal.skill_id.is_some());

    let skill_drawer_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE memory_kind = 'skill'",
            [],
            |row| row.get(0),
        )
        .expect("count skill drawers");
    assert_eq!(
        skill_drawer_count, 0,
        "case-backed proposals must not create skill drawer rows"
    );

    let skills = list_skills(db.conn(), Some("probationary"), None).expect("list skills");
    assert_eq!(skills.len(), 1);
    let skill = &skills[0];
    assert_eq!(Some(skill.skill_id.as_str()), proposal.skill_id.as_deref());
    assert_eq!(skill.status, SkillStatus::Probationary);
    let mut expected_supporting = vec![case_a, case_b];
    expected_supporting.sort();
    assert_eq!(skill.exemplar_ids, expected_supporting);
    assert!(
        skill
            .trigger_description
            .contains("Verification refs: drawer_verify_a, drawer_verify_b.")
    );
    assert!(skill.trigger_description.contains(&case_failed));
    assert!(
        skill
            .trigger_description
            .contains("do not infer repeated procedure without explicit key")
    );
    assert!(
        skill
            .trigger_description
            .contains("fuzzy grouping by prose summary")
    );
}

#[test]
fn test_concurrent_case_backed_proposals_reuse_single_live_skill() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open seed db");
    insert_evidence(&db, "drawer_verify_concurrent_a", "proposal test A passed");
    insert_evidence(&db, "drawer_verify_concurrent_b", "proposal test B passed");

    let case_a = open_test_case(&db, "case-skill.concurrent-propose", "first success");
    let case_b = open_test_case(&db, "case-skill.concurrent-propose", "second success");
    close_success(&db, &case_a, "drawer_verify_concurrent_a");
    close_success(&db, &case_b, "drawer_verify_concurrent_b");
    drop(db);

    let barrier = Arc::new(Barrier::new(2));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let db_path = db_path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            let db = Database::open(&db_path).expect("open concurrent db");
            barrier.wait();
            propose_skills_from_cases(
                &db,
                SkillProposalOptions {
                    from_cases: true,
                    min_support: 2,
                    min_verification_refs: 1,
                    wing: Some("mempal".to_string()),
                    project_id: None,
                    dry_run: false,
                },
            )
            .expect("concurrent proposal")
        }));
    }

    let mut skill_ids = Vec::new();
    let mut pattern_ids = Vec::new();
    for handle in handles {
        let batch = handle.join().expect("proposal thread");
        assert_eq!(batch.proposals.len(), 1);
        let proposal = &batch.proposals[0];
        skill_ids.push(proposal.skill_id.clone().expect("skill id"));
        pattern_ids.push(proposal.pattern_id.clone());
    }
    assert_eq!(pattern_ids[0], pattern_ids[1]);
    assert_eq!(
        skill_ids[0], skill_ids[1],
        "concurrent proposals should reuse the single live skill"
    );

    let db = Database::open(&db_path).expect("open verification db");
    let live_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM skills WHERE pattern_id = ?1 AND status IN ('probationary', 'active')",
            [&pattern_ids[0]],
            |row| row.get(0),
        )
        .expect("count live skills");
    assert_eq!(live_count, 1);
}

#[test]
fn test_case_backed_skill_surfaces_after_adoption_with_pattern_link() {
    let (_tmp, db) = new_db();
    insert_evidence(&db, "drawer_verify_surface_a", "surface test A passed");
    insert_evidence(&db, "drawer_verify_surface_b", "surface test B passed");

    let case_a = open_test_case(&db, "case-skill.surface", "first surface success");
    let case_b = open_test_case(&db, "case-skill.surface", "second surface success");
    close_success(&db, &case_a, "drawer_verify_surface_a");
    close_success(&db, &case_b, "drawer_verify_surface_b");

    let proposed = propose_skills_from_cases(
        &db,
        SkillProposalOptions {
            from_cases: true,
            min_support: 2,
            min_verification_refs: 1,
            wing: Some("mempal".to_string()),
            project_id: None,
            dry_run: false,
        },
    )
    .expect("propose case-backed skill");
    let proposal = proposed.proposals.first().expect("one proposal");
    let skill_id = proposal.skill_id.as_deref().expect("created skill id");

    let (pattern_status, pattern_model_id, signature): (String, Option<String>, Vec<u8>) = db
        .conn()
        .query_row(
            "SELECT status, model_id, signature FROM patterns WHERE pattern_id = ?1",
            [&proposal.pattern_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("load matching pattern row");
    assert_eq!(pattern_status, "active");
    assert_eq!(pattern_model_id.as_deref(), Some("case-backed-skill-v1"));
    assert!(!signature.is_empty(), "pattern signature should be usable");

    let status = adopt_skill(db.conn(), skill_id, 1).expect("adopt case-backed skill");
    assert_eq!(status, Some(SkillStatus::Active));

    let active_skills = load_active_skills_for_context(db.conn(), None, &[1.0, 0.0, 0.0], 0.70)
        .expect("load active skills");
    assert!(
        active_skills.iter().any(|skill| skill.skill_id == skill_id),
        "adopted case-backed skill should surface in active skill context"
    );
}

#[test]
fn test_case_close_keeps_cjk_fts_tokenization() {
    let (_tmp, db) = new_db();
    insert_evidence(&db, "drawer_verify_cjk", "中文检索验证通过");
    let case_id = open_case(
        &db,
        CaseOpenRequest {
            task: "中文初始任务".to_string(),
            procedure_key: "case-skill.cjk".to_string(),
            procedure_summary: "保持中文 FTS 分词一致".to_string(),
            procedure_steps: vec!["打开 case".to_string()],
            trajectory: vec!["初始中文轨迹".to_string()],
            anti_patterns: Vec::new(),
            failed_approaches: Vec::new(),
            wing: "mempal".to_string(),
            room: "cases".to_string(),
            project_id: None,
            importance: 3,
            dry_run: false,
        },
    )
    .expect("open cjk case")
    .case_id;

    close_case(
        &db,
        CaseCloseRequest {
            case_id: case_id.clone(),
            verdict: CaseVerdict::Success,
            tests: vec!["最终验证中文检索成功".to_string()],
            verification_refs: vec!["drawer_verify_cjk".to_string()],
            anti_patterns: Vec::new(),
            failed_approaches: Vec::new(),
        },
    )
    .expect("close cjk case");

    let ids: Vec<String> = db
        .search_fts(
            "最终验证中文检索",
            Some("mempal"),
            Some("cases"),
            "all",
            None,
            10,
        )
        .expect("search fts")
        .into_iter()
        .map(|(id, _rank)| id)
        .collect();
    assert!(
        ids.iter().any(|id| id == &case_id),
        "closed CJK case should remain searchable after FTS sync"
    );
}

#[test]
fn test_case_skill_cli_path_does_not_require_cloud_embedder() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create .mempal");
    let db_path = mempal_home.join("palace.db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[embed]
backend = "api"
api_endpoint = "http://127.0.0.1:9/v1/embeddings"
api_model = "offline-test"

[config_hot_reload]
enabled = false

[ingest_gating]
enabled = false

[project]
id = "proj-case-skill-cli"
"#,
            db_path.display()
        ),
    )
    .expect("write config");

    let db = Database::open(&db_path).expect("open db");
    insert_evidence(&db, "drawer_cli_verify", "cli deterministic verification");
    drop(db);

    let opened = run_mempal(
        &home,
        &[
            "case",
            "open",
            "--task",
            "cli case",
            "--procedure-key",
            "case-skill.cli",
            "--procedure",
            "Close case then propose skill",
            "--step",
            "open",
            "--trajectory",
            "cli path",
            "--json",
        ],
    );
    assert!(
        opened.status.success(),
        "case open failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&opened.stdout),
        String::from_utf8_lossy(&opened.stderr)
    );
    let case_id = parse_json_field(&opened.stdout, "case_id");

    let closed = run_mempal(
        &home,
        &[
            "case",
            "close",
            &case_id,
            "--verdict",
            "success",
            "--test",
            "offline deterministic CLI test",
            "--verification-ref",
            "drawer_cli_verify",
            "--json",
        ],
    );
    assert!(
        closed.status.success(),
        "case close failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&closed.stdout),
        String::from_utf8_lossy(&closed.stderr)
    );

    let proposed = run_mempal(
        &home,
        &[
            "skill",
            "propose",
            "--from-cases",
            "--min-support",
            "1",
            "--json",
        ],
    );
    assert!(
        proposed.status.success(),
        "skill propose failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&proposed.stdout),
        String::from_utf8_lossy(&proposed.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&proposed.stdout).expect("proposal json");
    assert_eq!(value["proposals"].as_array().expect("array").len(), 1);
    assert!(
        value["proposals"][0].get("drawer_id").is_none(),
        "case-backed skill proposals must not expose drawer ids"
    );
    assert!(value["proposals"][0]["skill_id"].is_string());

    let db = Database::open(&db_path).expect("open db after cli");
    assert_eq!(
        drawer_project_id(&db, &case_id).as_deref(),
        Some("proj-case-skill-cli")
    );
    let skills =
        list_skills(db.conn(), Some("probationary"), Some("proj-case-skill-cli")).expect("list");
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].project_id.as_deref(), Some("proj-case-skill-cli"));
}
