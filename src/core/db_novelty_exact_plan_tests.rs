use super::*;
use crate::core::types::{Drawer, SourceType};
use tempfile::TempDir;

const SQL_CURRENT: &str = r#"
            WITH recent_drawers AS MATERIALIZED (
                SELECT d.id
                FROM drawer_vectors_rowids r
                CROSS JOIN drawers d
                WHERE d.id = r.id
                  AND d.deleted_at IS NULL
                  AND (?2 IS NULL OR d.wing = ?2)
                  AND (?3 IS NULL OR d.room = ?3)
                  AND (?4 IS NULL OR d.project_id = ?4)
                ORDER BY r.rowid DESC
                LIMIT ?6
            )
            SELECT rd.id,
                   CAST(1.0 - vec_distance_cosine(v.embedding, vec_f32(?1)) AS REAL) AS similarity
            FROM recent_drawers rd
            JOIN drawer_vectors v ON v.id = rd.id
            ORDER BY similarity DESC
            LIMIT ?5
            "#;

const SQL_COUNT: &str = r#"
                SELECT COUNT(*)
                FROM drawers d
                JOIN drawer_vectors_rowids r ON r.id = d.id
                WHERE d.deleted_at IS NULL
                  AND (?1 IS NULL OR d.wing = ?1)
                  AND (?2 IS NULL OR d.room = ?2)
                  AND (?3 IS NULL OR d.project_id = ?3)
            "#;

fn test_drawer(id: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: format!("content for {id}"),
        wing: "code-memory".to_string(),
        room: Some("novelty".to_string()),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::AgentInference,
        added_at: "1700000000".to_string(),
        chunk_index: None,
        importance: 0,
        ..Drawer::default()
    }
}

fn seed_store(n: usize, dim: usize) -> (TempDir, Database, String) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("test.db")).expect("open db");
    for i in 0..n {
        let id = format!("d{i:04}");
        let mut vector = vec![0.0_f32; dim];
        vector[0] = (i as f32) + 1.0;
        db.insert_drawer_with_project(&test_drawer(&id), None)
            .expect("insert drawer");
        db.insert_vector_with_project(&id, &vector, None)
            .expect("insert vector");
    }
    let query_json = serde_json::to_string(&vec![1.0_f32; dim]).expect("query json");
    (tmp, db, query_json)
}

fn query_plan(db: &Database, sql: &str, params: impl rusqlite::Params) -> Vec<String> {
    let explain = format!("EXPLAIN QUERY PLAN {sql}");
    let mut statement = db.conn().prepare(&explain).expect("prepare explain");
    statement
        .query_map(params, |row| row.get::<_, String>(3))
        .expect("query plan")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect plan")
}

fn uses_vec0_vtab(plan: &[String]) -> bool {
    plan.iter().any(|detail| {
        let lower = detail.to_ascii_lowercase();
        lower.contains("drawer_vectors") && lower.contains("virtual table")
    })
}

#[test]
fn no_project_novelty_count_must_not_touch_vec0_vtab() {
    let (_tmp, db, _query_json) = seed_store(24, 8);
    let plan = query_plan(
        &db,
        SQL_COUNT,
        rusqlite::params!["code-memory", Option::<&str>::None, Option::<&str>::None],
    );
    eprintln!("COUNT_PLAN={plan:?}");
    assert!(
        !uses_vec0_vtab(&plan),
        "count_novelty_candidate_drawers must use drawer_vectors_rowids, not vec0 POINT blob loads; plan={plan:?}"
    );
    assert!(
        plan.iter()
            .any(|detail| detail.contains("drawer_vectors_rowids")),
        "count must join the vec0 rowids shadow table; plan={plan:?}"
    );
}

#[test]
fn novelty_candidates_exact_must_bound_ids_from_rowids_then_point_join() {
    let (_tmp, db, query_json) = seed_store(48, 8);
    let plan = query_plan(
        &db,
        SQL_CURRENT,
        rusqlite::params![
            query_json,
            "code-memory",
            Option::<&str>::None,
            Option::<&str>::None,
            5_i64,
            6_i64
        ],
    );
    eprintln!("CURRENT_PLAN={plan:?}");
    let plan_text = plan.join(" | ");
    assert!(
        !plan_text.contains("idx_drawers_deleted_at"),
        "bounded recent-id CTE must not walk live drawers via idx_drawers_deleted_at; plan={plan:?}"
    );
    assert!(
        plan.iter().any(|detail| detail.contains("SCAN r")),
        "bounded recent-id CTE must scan drawer_vectors_rowids (alias r); plan={plan:?}"
    );
    assert!(
        plan.iter()
            .any(|detail| detail.contains("INDEX 3:2!___") || detail.contains("INDEX 1:2!___")),
        "cosine JOIN must remain a vec0 POINT lookup over the limited IDs; plan={plan:?}"
    );
}
