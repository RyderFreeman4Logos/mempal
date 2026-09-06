use super::*;
use crate::core::patterns::get_pattern;

fn patterns_connection() -> Connection {
    let conn = Connection::open_in_memory().expect("open patterns DB");
    conn.execute_batch(
        "CREATE TABLE patterns (
            pattern_id TEXT PRIMARY KEY,
            signature BLOB NOT NULL,
            exemplar_ids TEXT NOT NULL,
            exemplar_count INTEGER NOT NULL,
            session_ids TEXT NOT NULL,
            session_count INTEGER NOT NULL,
            topic_tags TEXT,
            model_id TEXT,
            status TEXT NOT NULL,
            first_seen_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            project_id TEXT
        )",
    )
    .expect("create patterns table");
    conn
}

fn pattern(id: &str, exemplar: &str) -> NewPattern {
    NewPattern {
        pattern_id: id.to_string(),
        signature: vec![0.5; 8],
        exemplar_ids: vec![exemplar.to_string()],
        session_ids: vec!["session-a".to_string()],
        topic_tags: vec!["test".to_string()],
        model_id: Some("test-model".to_string()),
        project_id: None,
    }
}

#[test]
fn stale_pattern_plans_do_not_overwrite_or_create_duplicates() {
    let conn = patterns_connection();
    insert_pattern(&conn, &pattern("current", "seed")).expect("insert current pattern");
    let stale_revision = load_pattern_revision(&conn, "current")
        .expect("load revision")
        .expect("current pattern");
    update_pattern_with_exemplar(&conn, "current", "concurrent", "session-b", &[0.25; 8], 5)
        .expect("apply concurrent pattern update");
    let concurrent = get_pattern(&conn, "current")
        .expect("read concurrent pattern")
        .expect("updated pattern");
    let embedding = [0.75; 8];
    let args = PatternDetectionArgs {
        new_drawer_id: "stale",
        session_id: "session-c",
        embedding: &embedding,
        project_id: None,
        model_id: "test-model",
        similarity_threshold: 0.82,
        min_sessions: 3,
        min_exemplars: 3,
        promote_threshold: 5,
        top_tags: 5,
    };
    try_apply_pattern_detection(&conn, &args, PatternDetectionPlan::Update(stale_revision))
        .expect("skip stale update plan");
    let after_stale_update = get_pattern(&conn, "current")
        .expect("read pattern after stale update")
        .expect("pattern remains");
    assert_eq!(after_stale_update.exemplar_ids, concurrent.exemplar_ids);
    assert_eq!(after_stale_update.signature, concurrent.signature);

    let conn = patterns_connection();
    insert_pattern(&conn, &pattern("winner", "seed")).expect("insert concurrent winner");
    try_apply_pattern_detection(
        &conn,
        &args,
        PatternDetectionPlan::Insert {
            pattern: pattern("stale-candidate", "seed"),
            overlapping_exemplar_ids: vec!["seed".to_string()],
        },
    )
    .expect("skip stale insert plan");
    let count = conn
        .query_row("SELECT COUNT(*) FROM patterns", [], |row| {
            row.get::<_, i64>(0)
        })
        .expect("count patterns");
    assert_eq!(count, 1, "stale insert must not duplicate the winner");
}
