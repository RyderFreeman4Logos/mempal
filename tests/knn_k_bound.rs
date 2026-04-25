//! Integration test for issue #58: sqlite-vec KNN k must not exceed 4096.
//!
//! Populates a database with >4096 drawers + vectors, then runs
//! `search_by_vector` with scope=All to verify no "k value too large" error.

mod common;

use mempal::core::db::Database;
use mempal::core::project::ProjectSearchScope;
use mempal::core::types::{Drawer, RouteDecision, SourceType};
use mempal::search::{compute_knn_k, search_by_vector};
use tempfile::TempDir;

const DIM: usize = 4;
const DRAWER_COUNT: usize = 5_000;

fn make_drawer(i: usize) -> Drawer {
    Drawer {
        id: format!("d-{i:06}"),
        content: format!("content for drawer {i}"),
        wing: "test".to_string(),
        room: Some("room".to_string()),
        source_file: None,
        source_type: SourceType::Manual,
        added_at: "1700000000".to_string(),
        chunk_index: None,
        importance: 0,
    }
}

fn random_vector(seed: usize) -> Vec<f32> {
    // Deterministic pseudo-random vector seeded by drawer index.
    let mut v = vec![0.0f32; DIM];
    let mut state = seed as u64 ^ 0xDEAD_BEEF;
    for value in &mut v {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        *value = ((state >> 33) as f32) / (u32::MAX as f32);
    }
    v
}

#[test]
fn search_by_vector_succeeds_with_more_than_4096_drawers() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("test.db")).expect("open db");

    // Seed drawers + vectors
    for i in 0..DRAWER_COUNT {
        let drawer = make_drawer(i);
        db.insert_drawer_with_project(&drawer, None)
            .expect("insert drawer");
        db.insert_vector(&drawer.id, &random_vector(i))
            .expect("insert vector");
    }

    let route = RouteDecision {
        wing: None,
        room: None,
        confidence: 0.0,
        reason: "test".to_string(),
    };
    let scope = ProjectSearchScope::from_request(None, true, false, false); // all-projects

    let query_vec = random_vector(42);
    let results = search_by_vector(&db, &query_vec, route, &scope, 10);

    let results = results.expect("search_by_vector must not fail at drawer_count > 4096");
    assert!(
        !results.is_empty(),
        "expected >0 results from {DRAWER_COUNT} drawers"
    );
    assert!(
        results.len() <= 10,
        "expected at most 10 results, got {}",
        results.len()
    );
}

#[test]
fn search_by_vector_succeeds_with_20000_drawers() {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("test.db")).expect("open db");

    let count = 20_000usize;
    for i in 0..count {
        let drawer = make_drawer(i);
        db.insert_drawer_with_project(&drawer, None)
            .expect("insert drawer");
        db.insert_vector(&drawer.id, &random_vector(i))
            .expect("insert vector");
    }

    let route = RouteDecision {
        wing: None,
        room: None,
        confidence: 0.0,
        reason: "test".to_string(),
    };
    let scope = ProjectSearchScope::from_request(None, true, false, false);

    let results = search_by_vector(&db, &random_vector(99), route, &scope, 10)
        .expect("search_by_vector must not fail at drawer_count = 20000");
    assert!(
        !results.is_empty(),
        "expected >0 results from {count} drawers"
    );
}

#[test]
fn compute_knn_k_never_exceeds_sqlite_vec_limit() {
    for top_k in [0, 1, 10, 100, 1_000, 10_000] {
        let k = compute_knn_k(top_k);
        assert!(
            k <= 4_096,
            "compute_knn_k({top_k}) = {k}, exceeds 4096 limit"
        );
        assert!(k >= 100, "compute_knn_k({top_k}) = {k}, below 100 floor");
    }
}
