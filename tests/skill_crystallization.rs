#![warn(clippy::all)]

use std::path::PathBuf;
use std::process::Command;

use mempal::context::{ContextRequest, assemble_context};
use mempal::core::db::{
    Database, apply_fork_ext_migrations_to, read_fork_ext_version, set_fork_ext_version,
};
use mempal::core::patterns::{NewPattern, insert_pattern, promote_pattern, retire_pattern};
use mempal::core::skills::{
    PromoteArgs, PromotionError, SkillStatus, adopt_skill, compute_eta, get_skill, list_skills,
    promote_pattern_to_skill, reject_skill,
};
use mempal::core::types::MemoryDomain;
use rusqlite::Connection;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn new_db() -> (TempDir, PathBuf, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db_path, db)
}

/// Returns column names for a table via PRAGMA table_info.
fn column_names(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("prepare table_info");
    stmt.query_map([], |row| row.get::<_, String>(1))
        .expect("query table_info")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect column names")
}

/// Build a unit vector of given dimension (all values set to `val`).
fn unit_vec(dim: usize, val: f32) -> Vec<f32> {
    let norm = (dim as f32 * val * val).sqrt();
    if norm == 0.0 {
        return vec![0.0; dim];
    }
    vec![val / norm; dim]
}

/// Insert a pattern and promote it to active status.
fn insert_active_pattern(
    conn: &Connection,
    pattern_id: &str,
    session_count: usize,
    signature: Vec<f32>,
    project_id: Option<&str>,
) {
    let session_ids: Vec<String> = (0..session_count)
        .map(|i| format!("sess-{pattern_id}-{i}"))
        .collect();
    let exemplar_ids: Vec<String> = (0..session_count.min(3))
        .map(|i| format!("ex-{pattern_id}-{i}"))
        .collect();
    insert_pattern(
        conn,
        &NewPattern {
            pattern_id: pattern_id.to_string(),
            signature,
            exemplar_ids,
            session_ids,
            topic_tags: vec!["test".to_string()],
            model_id: Some("test-model".to_string()),
            project_id: project_id.map(str::to_string),
        },
    )
    .expect("insert pattern");
    promote_pattern(conn, pattern_id).expect("promote pattern to active");
}

struct StubEmbedder {
    vector: Vec<f32>,
}

#[async_trait::async_trait]
impl mempal::embed::Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }
    fn dimensions(&self) -> usize {
        self.vector.len()
    }
    fn name(&self) -> &str {
        "stub"
    }
}

// ---------------------------------------------------------------------------
// Scenario: fork-ext migration 12 → 13 creates skills table
// ---------------------------------------------------------------------------

#[test]
fn test_fork_ext_migration_v8_to_v9_creates_skills_table() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();

    // Roll back to version 12 (pre-skills).
    set_fork_ext_version(conn, 12).expect("set version 12");
    assert_eq!(read_fork_ext_version(conn).unwrap(), 12);

    // Apply up to version 13.
    apply_fork_ext_migrations_to(conn, 13).expect("apply v13 migration");

    assert_eq!(read_fork_ext_version(conn).unwrap(), 13);

    // Skills table must exist.
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skills'",
            [],
            |row| row.get(0),
        )
        .expect("check skills table");
    assert_eq!(count, 1, "skills table should exist");

    let cols = column_names(conn, "skills");
    for expected in &[
        "skill_id",
        "name",
        "trigger_description",
        "pattern_id",
        "adoption_count",
        "rejection_count",
        "status",
    ] {
        assert!(
            cols.iter().any(|c| c == expected),
            "missing column: {expected}"
        );
    }
    // eta must NOT be a stored column.
    assert!(
        !cols.iter().any(|c| c == "eta"),
        "eta must not be a stored column"
    );

    // idx_skills_status index must exist.
    let idx_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_skills_status'",
            [],
            |row| row.get(0),
        )
        .expect("check idx_skills_status");
    assert_eq!(idx_count, 1, "idx_skills_status index should exist");
}

// ---------------------------------------------------------------------------
// Scenario: active pattern with sufficient sessions → probationary skill
// ---------------------------------------------------------------------------

#[test]
fn test_pattern_promote_creates_probationary_skill() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();
    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-alpha", 6, sig, None);

    let args = PromoteArgs {
        pattern_id: "pat-alpha",
        name: "Deploy guard",
        trigger_description: "When about to deploy, verify tests pass first",
        skill_min_sessions: 5,
        project_id: None,
    };
    let skill = promote_pattern_to_skill(conn, &args).expect("promote ok");

    assert_eq!(skill.status, SkillStatus::Probationary);
    assert_eq!(skill.name, "Deploy guard");
    assert_eq!(
        skill.trigger_description,
        "When about to deploy, verify tests pass first"
    );
    assert_eq!(skill.adoption_count, 0);
    assert_eq!(skill.rejection_count, 0);
    assert_eq!(skill.pattern_id, "pat-alpha");

    // Verify persistence.
    let fetched = get_skill(conn, &skill.skill_id)
        .expect("get skill ok")
        .expect("skill should exist");
    assert_eq!(fetched.status, SkillStatus::Probationary);
}

// ---------------------------------------------------------------------------
// Scenario: candidate pattern cannot be promoted
// ---------------------------------------------------------------------------

#[test]
fn test_candidate_pattern_cannot_be_promoted() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();
    let sig = unit_vec(8, 1.0);
    // Insert pattern but do NOT promote it — stays candidate.
    insert_pattern(
        conn,
        &NewPattern {
            pattern_id: "pat-candidate".to_string(),
            signature: sig,
            exemplar_ids: vec![
                "e1".to_string(),
                "e2".to_string(),
                "e3".to_string(),
                "e4".to_string(),
                "e5".to_string(),
                "e6".to_string(),
            ],
            session_ids: (0..6).map(|i| format!("s{i}")).collect(),
            topic_tags: vec![],
            model_id: None,
            project_id: None,
        },
    )
    .expect("insert pattern");

    let args = PromoteArgs {
        pattern_id: "pat-candidate",
        name: "some skill",
        trigger_description: "some trigger",
        skill_min_sessions: 5,
        project_id: None,
    };
    let result = promote_pattern_to_skill(conn, &args);
    assert!(
        matches!(result, Err(PromotionError::PatternNotActive(_))),
        "expected PatternNotActive, got {result:?}"
    );

    // No skill row should have been created.
    let skills = list_skills(conn, None, None).expect("list skills");
    assert!(skills.is_empty(), "no skills should exist");
}

// ---------------------------------------------------------------------------
// Scenario: duplicate promotion rejected
// ---------------------------------------------------------------------------

#[test]
fn test_duplicate_promotion_rejected() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();
    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-dup", 6, sig, None);

    let args = PromoteArgs {
        pattern_id: "pat-dup",
        name: "First skill",
        trigger_description: "trigger one",
        skill_min_sessions: 5,
        project_id: None,
    };
    promote_pattern_to_skill(conn, &args).expect("first promotion ok");

    // Second promotion must fail.
    let args2 = PromoteArgs {
        pattern_id: "pat-dup",
        name: "Second skill",
        trigger_description: "trigger two",
        skill_min_sessions: 5,
        project_id: None,
    };
    let result = promote_pattern_to_skill(conn, &args2);
    assert!(
        matches!(result, Err(PromotionError::SkillAlreadyExists)),
        "expected SkillAlreadyExists, got {result:?}"
    );

    // Still only one skill.
    let skills = list_skills(conn, None, None).expect("list skills");
    assert_eq!(skills.len(), 1);
}

// ---------------------------------------------------------------------------
// Scenario: adoption reaches active_threshold → skill promoted to active
// ---------------------------------------------------------------------------

#[test]
fn test_skill_adopts_to_active_at_threshold() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();
    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-adopt", 6, sig, None);

    let args = PromoteArgs {
        pattern_id: "pat-adopt",
        name: "Adopt guard",
        trigger_description: "trigger",
        skill_min_sessions: 5,
        project_id: None,
    };
    let skill = promote_pattern_to_skill(conn, &args).expect("promote ok");

    let active_threshold = 3_i64;

    // First two adoptions — still probationary.
    for _ in 0..2 {
        let status = adopt_skill(conn, &skill.skill_id, active_threshold)
            .expect("adopt ok")
            .expect("skill exists");
        assert_eq!(status, SkillStatus::Probationary);
    }

    // Third adoption — must flip to active.
    let status = adopt_skill(conn, &skill.skill_id, active_threshold)
        .expect("adopt ok")
        .expect("skill exists");
    assert_eq!(
        status,
        SkillStatus::Active,
        "should be active after 3 adoptions"
    );

    let fetched = get_skill(conn, &skill.skill_id)
        .expect("get ok")
        .expect("exists");
    assert_eq!(fetched.adoption_count, 3);
    assert_eq!(fetched.status, SkillStatus::Active);
}

// ---------------------------------------------------------------------------
// Scenario: sufficient rejections with no adoptions → auto-retire
// ---------------------------------------------------------------------------

#[test]
fn test_skill_auto_retires_on_rejection() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();
    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-reject", 6, sig, None);

    let args = PromoteArgs {
        pattern_id: "pat-reject",
        name: "Reject guard",
        trigger_description: "trigger",
        skill_min_sessions: 5,
        project_id: None,
    };
    let skill = promote_pattern_to_skill(conn, &args).expect("promote ok");

    let retire_threshold = 3_i64;

    // First two rejections — still probationary.
    for _ in 0..2 {
        let status = reject_skill(conn, &skill.skill_id, retire_threshold)
            .expect("reject ok")
            .expect("skill exists");
        assert_eq!(status, SkillStatus::Probationary);
    }

    // Third rejection — no adoptions → auto-retire.
    let status = reject_skill(conn, &skill.skill_id, retire_threshold)
        .expect("reject ok")
        .expect("skill exists");
    assert_eq!(
        status,
        SkillStatus::Retired,
        "should auto-retire after 3 rejections with 0 adoptions"
    );
}

// ---------------------------------------------------------------------------
// Scenario: eta calculation (unit test)
// ---------------------------------------------------------------------------

#[test]
fn test_skill_eta_calculation() {
    // 3 adoptions, 1 rejection → 3.0 / (3 + 1 + 1.0) = 0.6
    let eta = compute_eta(3, 1);
    let expected = 3.0_f64 / 5.0_f64;
    assert!(
        (eta - expected).abs() < 1e-9,
        "eta expected {expected}, got {eta}"
    );

    // 0 adoptions, 0 rejections → 0.0 / 1.0 = 0.0
    let eta_zero = compute_eta(0, 0);
    assert!((eta_zero - 0.0).abs() < 1e-9, "zero eta expected");
}

// ---------------------------------------------------------------------------
// Scenario: active skill injected in T1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_active_skill_injected_in_t1() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();

    // Create an active pattern + active skill.
    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-t1", 6, sig.clone(), None);

    let args = PromoteArgs {
        pattern_id: "pat-t1",
        name: "T1 skill",
        trigger_description: "Apply at session start",
        skill_min_sessions: 5,
        project_id: None,
    };
    let skill = promote_pattern_to_skill(conn, &args).expect("promote ok");

    // Manually activate the skill.
    conn.execute(
        "UPDATE skills SET status = 'active', adoption_count = 3 WHERE skill_id = ?1",
        rusqlite::params![skill.skill_id],
    )
    .expect("activate skill");

    // Use a stub embedder that returns the same vector as the pattern signature
    // so cosine similarity = 1.0 (above 0.70 threshold).
    let embedder = StubEmbedder { vector: sig };

    let request = ContextRequest {
        query: "session start".to_string(),
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        cwd: _db_path.parent().unwrap().to_path_buf(),
        include_evidence: false,
        include_cards: false,
        max_items: 12,
        dao_tian_limit: 1,
        project_id: None,
        trigger: None,
        context_cfg_override: None,
    };

    let pack = assemble_context(&db, &embedder, request)
        .await
        .expect("assemble context ok");

    // Active skills should be injected at T1.
    assert!(
        !pack.active_skills.is_empty(),
        "active_skills should be non-empty"
    );
    let found = pack
        .active_skills
        .iter()
        .any(|s| s.skill_id == skill.skill_id);
    assert!(
        found,
        "skill {} should appear in active_skills",
        skill.skill_id
    );
}

// ---------------------------------------------------------------------------
// Scenario: probationary skill NOT injected in T1
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_probationary_skill_excluded_from_context() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();

    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-prob", 6, sig.clone(), None);

    let args = PromoteArgs {
        pattern_id: "pat-prob",
        name: "Prob skill",
        trigger_description: "not active yet",
        skill_min_sessions: 5,
        project_id: None,
    };
    let skill = promote_pattern_to_skill(conn, &args).expect("promote ok");
    // Skill remains probationary.
    assert_eq!(skill.status, SkillStatus::Probationary);

    let embedder = StubEmbedder { vector: sig };
    let request = ContextRequest {
        query: "session start".to_string(),
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        cwd: _db_path.parent().unwrap().to_path_buf(),
        include_evidence: false,
        include_cards: false,
        max_items: 12,
        dao_tian_limit: 1,
        project_id: None,
        trigger: None,
        context_cfg_override: None,
    };

    let pack = assemble_context(&db, &embedder, request)
        .await
        .expect("assemble context ok");

    let found = pack
        .active_skills
        .iter()
        .any(|s| s.skill_id == skill.skill_id);
    assert!(
        !found,
        "probationary skill should not appear in active_skills"
    );
}

// ---------------------------------------------------------------------------
// Scenario: retiring pattern does NOT cascade retire to skill
// ---------------------------------------------------------------------------

#[test]
fn test_pattern_retire_does_not_cascade_to_skill() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();

    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-cascade", 6, sig, None);

    let args = PromoteArgs {
        pattern_id: "pat-cascade",
        name: "Cascade skill",
        trigger_description: "trigger",
        skill_min_sessions: 5,
        project_id: None,
    };
    let skill = promote_pattern_to_skill(conn, &args).expect("promote ok");

    // Activate skill manually.
    conn.execute(
        "UPDATE skills SET status = 'active' WHERE skill_id = ?1",
        rusqlite::params![skill.skill_id],
    )
    .expect("activate skill");

    // Retire the pattern.
    let retired = retire_pattern(conn, "pat-cascade").expect("retire pattern ok");
    assert!(retired, "pattern should have been found and retired");

    // Verify skill is still active.
    let fetched = get_skill(conn, &skill.skill_id)
        .expect("get skill ok")
        .expect("skill exists");
    assert_eq!(
        fetched.status,
        SkillStatus::Active,
        "skill must remain active after pattern retirement"
    );
}

// ---------------------------------------------------------------------------
// Scenario: list_skills filters by project_id
// ---------------------------------------------------------------------------

#[test]
fn test_skill_list_filters_by_project() {
    let (_tmp, _db_path, db) = new_db();
    let conn = db.conn();

    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-px", 6, sig.clone(), Some("proj-X"));
    insert_active_pattern(conn, "pat-py", 6, sig, Some("proj-Y"));

    let args_x = PromoteArgs {
        pattern_id: "pat-px",
        name: "Skill X",
        trigger_description: "trigger",
        skill_min_sessions: 5,
        project_id: Some("proj-X"),
    };
    let args_y = PromoteArgs {
        pattern_id: "pat-py",
        name: "Skill Y",
        trigger_description: "trigger",
        skill_min_sessions: 5,
        project_id: Some("proj-Y"),
    };
    let skill_x = promote_pattern_to_skill(conn, &args_x).expect("promote x ok");
    promote_pattern_to_skill(conn, &args_y).expect("promote y ok");

    let result = list_skills(conn, None, Some("proj-X")).expect("list ok");
    assert_eq!(result.len(), 1, "only proj-X skill should appear");
    assert_eq!(result[0].skill_id, skill_x.skill_id);
}

// ---------------------------------------------------------------------------
// Scenario: skills promote CLI command works end-to-end
// ---------------------------------------------------------------------------

#[test]
fn test_skills_promote_cli() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create .mempal dir");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
db_path = "{}"

[embed]
backend = "model2vec"

[config_hot_reload]
enabled = false

[ingest_gating]
enabled = false

[patterns]
enabled = true

[skills]
skill_min_sessions = 5
active_threshold = 3
retire_threshold = 3
"#,
            db_path.display()
        ),
    )
    .expect("write config");

    let db = Database::open(&db_path).expect("open db");
    let conn = db.conn();
    let sig = unit_vec(8, 1.0);
    insert_active_pattern(conn, "pat-cli", 6, sig, None);
    drop(db);

    let bin = env!("CARGO_BIN_EXE_mempal");
    let output = Command::new(bin)
        .env("HOME", &home)
        .args([
            "skills",
            "promote",
            "pat-cli",
            "--name",
            "Test guard",
            "--trigger",
            "Run tests before deploy",
        ])
        .output()
        .expect("run mempal skills promote");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "mempal skills promote failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains("probationary") || stdout.contains("skill"),
        "expected success message, got: {stdout}"
    );

    // Verify skill was created in DB.
    let db2 = Database::open(&db_path).expect("open db after promote");
    let skills = list_skills(db2.conn(), None, None).expect("list skills");
    assert_eq!(skills.len(), 1, "one skill should exist");
    assert_eq!(skills[0].name, "Test guard");
    assert_eq!(skills[0].trigger_description, "Run tests before deploy");
}
