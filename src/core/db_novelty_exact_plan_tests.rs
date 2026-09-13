use super::*;
use crate::core::types::{Drawer, SourceType};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

const ORIGINAL_COUNT_SQL: &str = r#"
    SELECT COUNT(*)
    FROM drawer_vectors v
    JOIN drawers d ON d.id = v.id
    WHERE d.deleted_at IS NULL
      AND (?1 IS NULL OR d.wing = ?1)
      AND (?2 IS NULL OR d.room = ?2)
      AND (?3 IS NULL OR d.project_id = ?3)
"#;

const ORIGINAL_CANDIDATES_SQL: &str = r#"
    WITH recent_drawers AS (
        SELECT d.id
        FROM drawers d
        WHERE d.deleted_at IS NULL
          AND (?2 IS NULL OR d.wing = ?2)
          AND (?3 IS NULL OR d.room = ?3)
          AND (?4 IS NULL OR d.project_id = ?4)
          AND EXISTS (SELECT 1 FROM drawer_vectors v WHERE v.id = d.id)
        ORDER BY d.rowid DESC
        LIMIT ?6
    )
    SELECT rd.id,
           CAST(1.0 - vec_distance_cosine(v.embedding, vec_f32(?1)) AS REAL) AS similarity
    FROM recent_drawers rd
    JOIN drawer_vectors v ON v.id = rd.id
    ORDER BY similarity DESC
    LIMIT ?5
"#;

#[derive(Debug)]
struct QueryCost {
    cache_misses: i32,
    vm_steps: usize,
}

fn test_drawer(id: &str, wing: &str, room: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: format!("content for {id}"),
        wing: wing.to_string(),
        room: Some(room.to_string()),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::AgentInference,
        added_at: "1700000000".to_string(),
        chunk_index: None,
        importance: 0,
        ..Drawer::default()
    }
}

fn insert_drawer(db: &Database, id: &str, wing: &str, room: &str) {
    db.insert_drawer_with_project(&test_drawer(id, wing, room), None)
        .expect("insert drawer");
}

fn cache_misses(db: &Database) -> i32 {
    let mut current = 0;
    let mut highwater = 0;
    // SAFETY: the connection owns this live handle for the duration of the call,
    // and SQLite writes only to the two valid out-parameters.
    let result = unsafe {
        rusqlite::ffi::sqlite3_db_status(
            db.conn().handle(),
            rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS,
            &mut current,
            &mut highwater,
            0,
        )
    };
    assert_eq!(result, rusqlite::ffi::SQLITE_OK, "read cache misses");
    current
}

fn measure<T>(path: &std::path::Path, query: impl FnOnce(&Database) -> T) -> (T, QueryCost) {
    // Keep each arm independent of pages retained by the seeding connection.
    let db = Database::open(path).expect("open fresh cost snapshot");
    db.conn()
        .execute_batch("PRAGMA cache_size = -64; PRAGMA shrink_memory;")
        .expect("set SQLite page-cache budget");
    let misses_before = cache_misses(&db);
    let steps = Arc::new(AtomicUsize::new(0));
    let counted_steps = Arc::clone(&steps);
    db.conn().progress_handler(
        1,
        Some(move || {
            counted_steps.fetch_add(1, Ordering::Relaxed);
            false
        }),
    );
    let result = query(&db);
    db.conn().progress_handler(0, None::<fn() -> bool>);
    (
        result,
        QueryCost {
            cache_misses: cache_misses(&db) - misses_before,
            vm_steps: steps.load(Ordering::Relaxed),
        },
    )
}

fn original_count(db: &Database, wing: Option<&str>, room: Option<&str>) -> i64 {
    db.conn()
        .query_row(
            ORIGINAL_COUNT_SQL,
            (wing, room, Option::<&str>::None),
            |row| row.get(0),
        )
        .expect("run original count query")
}

fn original_candidates(
    db: &Database,
    query_vector: &[f32],
    limit: usize,
    scan_limit: usize,
) -> Vec<(String, f32)> {
    let query_json = serde_json::to_string(query_vector).expect("query json");
    let mut statement = db
        .conn()
        .prepare(ORIGINAL_CANDIDATES_SQL)
        .expect("prepare original candidate query");
    statement
        .query_map(
            rusqlite::params![
                query_json,
                "code-memory",
                "novelty",
                Option::<&str>::None,
                limit as i64,
                scan_limit as i64,
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?)),
        )
        .expect("run original candidate query")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect original candidates")
}

fn query_plan<P: rusqlite::Params>(db: &Database, sql: &str, params: P) -> Vec<String> {
    let mut statement = db
        .conn()
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("prepare query plan");
    statement
        .query_map(params, |row| row.get::<_, String>(3))
        .expect("query plan")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect query plan")
}

fn referenced_tables(db: &Database, sql: &str) -> HashSet<String> {
    let tables = Arc::new(Mutex::new(HashSet::new()));
    let captured = Arc::clone(&tables);
    db.conn().authorizer(Some(move |context: AuthContext<'_>| {
        if let AuthAction::Read { table_name, .. } = context.action {
            captured
                .lock()
                .expect("lock referenced tables")
                .insert(table_name.to_string());
        }
        Authorization::Allow
    }));
    let statement = db.conn().prepare(sql).expect("prepare production SQL");
    drop(statement);
    db.conn()
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    Arc::into_inner(tables)
        .expect("release table observer")
        .into_inner()
        .expect("read referenced tables")
}

#[test]
fn novelty_candidates_keep_drawer_recency_after_vector_replacement() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("test.db")).expect("open db");
    for (id, wing, room) in [
        ("old-perfect", "code-memory", "novelty"),
        ("recent-low", "code-memory", "novelty"),
        ("newest-medium", "code-memory", "novelty"),
        ("newer-missing-vector", "code-memory", "novelty"),
        ("newer-deleted", "code-memory", "novelty"),
        ("newer-other-wing", "other-wing", "novelty"),
        ("newer-other-room", "code-memory", "other-room"),
    ] {
        insert_drawer(&db, id, wing, room);
    }
    for (id, vector) in [
        ("old-perfect", [1.0, 0.0]),
        ("recent-low", [0.0, 1.0]),
        ("newest-medium", [0.8, 0.6]),
        ("newer-deleted", [1.0, 0.0]),
        ("newer-other-wing", [1.0, 0.0]),
        ("newer-other-room", [1.0, 0.0]),
    ] {
        db.insert_vector(id, &vector).expect("insert vector");
    }
    db.soft_delete_drawer("newer-deleted")
        .expect("soft-delete drawer");
    db.upsert_drawer_and_replace_vector(
        &test_drawer("old-perfect", "code-memory", "novelty"),
        &[1.0, 0.0],
    )
    .expect("replace oldest drawer vector");

    assert_eq!(
        db.count_novelty_candidate_drawers(Some("code-memory"), Some("novelty"), None)
            .expect("count candidates"),
        3
    );
    let candidate_ids = || {
        db.novelty_candidates_exact(
            &[1.0, 0.0],
            Some("code-memory"),
            Some("novelty"),
            None,
            2,
            2,
        )
        .expect("select candidates")
        .into_iter()
        .map(|(id, _)| id)
        .collect::<Vec<_>>()
    };
    assert_eq!(candidate_ids(), ["newest-medium", "recent-low"]);

    db.conn()
        .execute_batch("DROP TABLE drawer_vectors")
        .expect("drop vectors for rebuild");
    for (id, vector) in [
        ("newest-medium", [0.8, 0.6]),
        ("recent-low", [0.0, 1.0]),
        ("newer-other-room", [1.0, 0.0]),
        ("newer-other-wing", [1.0, 0.0]),
        ("newer-deleted", [1.0, 0.0]),
        ("old-perfect", [1.0, 0.0]),
    ] {
        db.insert_vector(id, &vector).expect("rebuild vector");
    }
    assert_eq!(candidate_ids(), ["newest-medium", "recent-low"]);
}

#[test]
fn novelty_actual_sut_has_bounded_plan_and_reduces_vm_work_from_original() {
    const DRAWERS: usize = 1536;
    const DIM: usize = 32;
    const LIMIT: usize = 16;
    let tmp = TempDir::new().expect("tempdir");
    let path = tmp.path().join("cost.db");
    {
        let db = Database::open(&path).expect("open db");
        assert_eq!(rusqlite::version(), "3.50.2");
        db.conn().execute_batch("BEGIN").expect("begin seed");
        for index in 0..DRAWERS {
            let id = format!("d{index:04}");
            insert_drawer(&db, &id, "code-memory", "novelty");
            let mut vector = vec![0.0_f32; DIM];
            vector[0] = 1.0;
            vector[1] = (index % 7) as f32;
            db.insert_vector(&id, &vector).expect("insert vector");
        }
        db.conn().execute_batch("COMMIT").expect("commit seed");
    }
    let query = vec![1.0_f32; DIM];

    let plan_db = Database::open(&path).expect("open plan snapshot");
    let count_plan = query_plan(
        &plan_db,
        NOVELTY_CANDIDATE_COUNT_SQL,
        (Some("code-memory"), Some("novelty"), Option::<&str>::None),
    );
    let count_tables = referenced_tables(&plan_db, NOVELTY_CANDIDATE_COUNT_SQL);
    let query_json = serde_json::to_string(&query).expect("query json");
    let candidate_plan = query_plan(
        &plan_db,
        NOVELTY_CANDIDATES_EXACT_SQL,
        rusqlite::params![
            query_json,
            "code-memory",
            "novelty",
            Option::<&str>::None,
            LIMIT as i64,
            LIMIT as i64,
        ],
    );
    let candidate_tables = referenced_tables(&plan_db, NOVELTY_CANDIDATES_EXACT_SQL);
    assert!(
        count_tables.contains("drawer_vectors_rowids")
            && !count_tables.contains("drawer_vectors")
            && count_plan
                .iter()
                .all(|detail| !detail.contains("VIRTUAL TABLE")),
        "count must use rowid metadata without touching vec0: tables={count_tables:?}, plan={count_plan:?}"
    );
    assert!(
        candidate_tables.contains("drawer_vectors_rowids")
            && candidate_tables.contains("drawer_vectors")
            && candidate_plan
                .iter()
                .any(|detail| detail.contains("MATERIALIZE recent_drawers"))
            && candidate_plan
                .iter()
                .filter(|detail| detail.contains("VIRTUAL TABLE"))
                .count()
                == 1,
        "candidate plan must materialize bounded drawer recency before one final vec0 lookup: tables={candidate_tables:?}, plan={candidate_plan:?}"
    );
    drop(plan_db);

    let (baseline_count, baseline_count_cost) = measure(&path, |db| {
        original_count(db, Some("code-memory"), Some("novelty"))
    });
    let (actual_count, actual_count_cost) = measure(&path, |db| {
        db.count_novelty_candidate_drawers(Some("code-memory"), Some("novelty"), None)
            .expect("actual count")
    });
    let (baseline_rows, baseline_candidate_cost) =
        measure(&path, |db| original_candidates(db, &query, LIMIT, LIMIT));
    let (actual_rows, actual_candidate_cost) = measure(&path, |db| {
        db.novelty_candidates_exact(
            &query,
            Some("code-memory"),
            Some("novelty"),
            None,
            LIMIT,
            LIMIT,
        )
        .expect("actual candidates")
    });

    eprintln!(
        "NOVELTY_COST sqlite={} sqlite_vec=0.1.9 drawers={DRAWERS} dim={DIM} \
         count_original_cache_misses={} count_actual_cache_misses={} \
         candidates_original_cache_misses={} candidates_actual_cache_misses={} \
         count_original_vm_steps={} count_actual_vm_steps={} \
         candidates_original_vm_steps={} candidates_actual_vm_steps={}",
        rusqlite::version(),
        baseline_count_cost.cache_misses,
        actual_count_cost.cache_misses,
        baseline_candidate_cost.cache_misses,
        actual_candidate_cost.cache_misses,
        baseline_count_cost.vm_steps,
        actual_count_cost.vm_steps,
        baseline_candidate_cost.vm_steps,
        actual_candidate_cost.vm_steps,
    );
    assert_eq!(actual_count, baseline_count);
    assert_eq!(actual_rows, baseline_rows);
    assert!(
        actual_count_cost.vm_steps < baseline_count_cost.vm_steps,
        "shadow-rowid count must reduce VM work: original={baseline_count_cost:?}, actual={actual_count_cost:?}"
    );
    assert!(
        actual_candidate_cost.vm_steps < baseline_candidate_cost.vm_steps,
        "bounded SUT must reduce VM work: original={baseline_candidate_cost:?}, actual={actual_candidate_cost:?}"
    );
}
