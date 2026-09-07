use async_trait::async_trait;
use mempal::core::config::ConfigHandle;
use mempal::core::db::{Database, DbError};
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::{EmbedError, Embedder};
use mempal::ingest::{IngestError, IngestOptions, ingest_file_with_options_and_writer_lease};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::sync_channel;
use std::time::Duration;
use tempfile::tempdir;

struct FixedEmbedder(Vec<f32>);

#[async_trait]
impl Embedder for FixedEmbedder {
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| self.0.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        self.0.len()
    }

    fn name(&self) -> &str {
        "fixed"
    }
}

fn seed_drawer(db: &Database, id: &str, source: &str, vector: &[f32]) {
    db.insert_drawer(&Drawer {
        id: id.to_string(),
        content: format!("pattern seed {id}"),
        wing: "test".to_string(),
        source_file: Some(source.to_string()),
        source_type: SourceType::AgentInference,
        added_at: "1713000000".to_string(),
        ..Drawer::default()
    })
    .expect("insert seed drawer");
    db.insert_vector(id, vector).expect("insert seed vector");
}

#[test]
fn pattern_planning_allows_a_concurrent_writer_and_apply_rejects_a_stale_lease() {
    let tmp = tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"db_path = "{}"

[embed]
backend = "stub"

[config_hot_reload]
enabled = false

[ingest_gating]
enabled = false

[patterns]
enabled = true
similarity_threshold = 0.82
min_sessions = 3
min_exemplars = 3
promote_threshold = 5
retire_after_days = 90
surfacing_threshold = 0.75
pattern_boost = 0.2
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");

    let db = Database::open(&db_path).expect("open db");
    assert_eq!(
        db.conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
            .expect("read journal mode"),
        "wal"
    );
    let vector = vec![0.5; 8];
    seed_drawer(&db, "seed-a", "a.md", &vector);
    seed_drawer(&db, "seed-b", "b.md", &vector);
    seed_drawer(&db, "seed-c", "c.md", &vector);
    let lease = db
        .runtime_writer_lease_acquire("sqlite-writer", "test-owner", "test-session", 300, None)
        .expect("acquire writer lease")
        .expect("writer lease available");

    let source = tmp.path().join("new.md");
    fs::write(&source, "new pattern evidence").expect("write source");

    let in_pattern_knn = Arc::new(AtomicBool::new(false));
    let paused_once = Arc::new(AtomicBool::new(false));
    let authorizer_flag = Arc::clone(&in_pattern_knn);
    db.conn().authorizer(Some(move |context: AuthContext<'_>| {
        if matches!(
            context.action,
            AuthAction::Read {
                table_name: "drawer_vectors",
                column_name: "project_id"
            }
        ) {
            authorizer_flag.store(true, Ordering::Release);
        }
        Authorization::Allow
    }));

    let (paused_tx, paused_rx) = sync_channel(0);
    let (release_tx, release_rx) = sync_channel(0);
    let progress_flag = Arc::clone(&in_pattern_knn);
    let progress_once = Arc::clone(&paused_once);
    db.conn().progress_handler(
        1,
        Some(move || {
            if progress_flag.load(Ordering::Acquire) && !progress_once.swap(true, Ordering::AcqRel)
            {
                paused_tx.send(()).expect("announce paused pattern query");
                release_rx.recv().expect("release paused pattern query");
            }
            false
        }),
    );

    let worker = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime")
            .block_on(ingest_file_with_options_and_writer_lease(
                &db,
                &FixedEmbedder(vector),
                &source,
                "test",
                IngestOptions::default(),
                Some(&lease),
            ))
    });

    paused_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("pattern KNN query must reach deterministic checkpoint");
    let contender = Database::open_with_busy_timeout(&db_path, Duration::ZERO)
        .expect("open fail-fast contender");
    let begin_result = contender.conn().execute_batch("BEGIN IMMEDIATE");
    if let Err(error) = begin_result {
        release_tx.send(()).expect("release RED worker");
        let _ = worker.join();
        panic!("read-only pattern planning must not hold the write transaction: {error}");
    }
    contender
        .conn()
        .execute(
            "DELETE FROM runtime_writer_leases WHERE name = 'sqlite-writer'",
            [],
        )
        .expect("invalidate lease while planning is paused");
    contender
        .conn()
        .execute_batch("COMMIT")
        .expect("commit lease invalidation");
    release_tx.send(()).expect("release pattern query");

    let error = worker
        .join()
        .expect("join ingest worker")
        .expect_err("stale lease must reject pattern apply");
    assert!(matches!(
        error,
        IngestError::InsertDrawer {
            source: DbError::RuntimeWriterLeaseLost { .. },
            ..
        }
    ));
}
