use std::thread;

use async_trait::async_trait;
use mempal::core::db::Database;
use mempal::embed::{Embedder, Result as EmbedResult};
use mempal::ingest::diary::{
    DAILY_ROLLUP_LIMIT_BYTES, DIARY_ROLLUP_WING, DiaryRollupOptions, diary_rollup_drawer_id,
    ingest_diary_rollup,
};
use mempal::ingest::{IngestError, IngestOptions, ingest_file_with_options};
use tempfile::TempDir;

struct LengthEmbedder;

#[async_trait]
impl Embedder for LengthEmbedder {
    async fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                vec![
                    text.len() as f32,
                    text.bytes().map(u32::from).sum::<u32>() as f32,
                ]
            })
            .collect())
    }

    fn dimensions(&self) -> usize {
        2
    }

    fn name(&self) -> &str {
        "length"
    }
}

fn new_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    (tmp, db)
}

async fn rollup(
    db: &Database,
    content: &str,
    room: &str,
    day: &str,
) -> mempal::ingest::diary::DiaryRollupOutcome {
    ingest_diary_rollup(
        db,
        &LengthEmbedder,
        content,
        DIARY_ROLLUP_WING,
        DiaryRollupOptions {
            room: Some(room),
            day: Some(day),
            dry_run: false,
            importance: 0,
        },
    )
    .await
    .expect("rollup ingest")
}

fn vector_json(db: &Database, drawer_id: &str) -> String {
    db.conn()
        .query_row(
            "SELECT vec_to_json(embedding) FROM drawer_vectors WHERE id = ?1",
            [drawer_id],
            |row| row.get::<_, String>(0),
        )
        .expect("vector json")
}

#[tokio::test]
async fn test_first_rollup_creates_day_drawer() {
    let (_tmp, db) = new_db();
    let day = "2026-04-24";

    let outcome = rollup(&db, "OBSERVATION: foo", "claude", day).await;

    assert_eq!(outcome.drawer_id, diary_rollup_drawer_id("claude", day));
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
    assert!(
        db.get_drawer(&outcome.drawer_id)
            .expect("get drawer")
            .is_some()
    );
}

#[tokio::test]
async fn test_second_rollup_same_day_appends() {
    let (_tmp, db) = new_db();
    let day = "2026-04-24";

    let first = rollup(&db, "A", "claude", day).await;
    let second = rollup(&db, "B", "claude", day).await;

    assert_eq!(first.drawer_id, second.drawer_id);
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
    let drawer = db
        .get_drawer(&first.drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert_eq!(drawer.content, "A\n\n---\n\nB");
}

#[tokio::test]
async fn test_different_day_creates_new_rollup() {
    let (_tmp, db) = new_db();

    rollup(&db, "A", "claude", "2026-04-16").await;
    let second = rollup(&db, "B", "claude", "2026-04-17").await;

    assert_eq!(db.drawer_count().expect("drawer count"), 2);
    assert!(second.drawer_id.contains("2026-04-17"));
}

#[tokio::test]
async fn test_different_room_separate_rollup() {
    let (_tmp, db) = new_db();
    let day = "2026-04-24";

    let claude = rollup(&db, "A", "claude", day).await;
    let codex = rollup(&db, "B", "codex", day).await;

    assert_eq!(db.drawer_count().expect("drawer count"), 2);
    assert!(claude.drawer_id.contains("claude"));
    assert!(codex.drawer_id.contains("codex"));
}

#[tokio::test]
async fn test_rollup_wrong_wing_rejected() {
    let (_tmp, db) = new_db();

    let error = ingest_diary_rollup(
        &db,
        &LengthEmbedder,
        "A",
        "mempal",
        DiaryRollupOptions {
            room: Some("claude"),
            day: Some("2026-04-24"),
            dry_run: false,
            importance: 0,
        },
    )
    .await
    .expect_err("wrong wing should fail");

    assert!(matches!(error, IngestError::DiaryRollupWrongWing { .. }));
    assert_eq!(db.drawer_count().expect("drawer count"), 0);
}

#[tokio::test]
async fn test_rollup_over_limit_rejected() {
    let (_tmp, db) = new_db();
    let day = "2026-04-24";
    let almost_full = "A".repeat(DAILY_ROLLUP_LIMIT_BYTES - 50);
    let first = rollup(&db, &almost_full, "claude", day).await;

    let error = ingest_diary_rollup(
        &db,
        &LengthEmbedder,
        &"B".repeat(100),
        DIARY_ROLLUP_WING,
        DiaryRollupOptions {
            room: Some("claude"),
            day: Some(day),
            dry_run: false,
            importance: 0,
        },
    )
    .await
    .expect_err("over limit should fail");

    assert!(matches!(error, IngestError::DailyRollupFull { .. }));
    let drawer = db
        .get_drawer(&first.drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    assert_eq!(drawer.content, almost_full);
}

#[tokio::test]
async fn test_rollup_vector_refreshed_on_upsert() {
    let (_tmp, db) = new_db();
    let day = "2026-04-24";

    let first = rollup(&db, "A", "claude", day).await;
    let vector_before = vector_json(&db, &first.drawer_id);
    rollup(&db, "B", "claude", day).await;
    let vector_after = vector_json(&db, &first.drawer_id);

    assert_ne!(vector_before, vector_after);
}

#[test]
fn test_concurrent_rollup_same_day_serialized() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("init db");

    let path_a = db_path.clone();
    let path_b = db_path.clone();
    let handle_a = thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async move {
            let db = Database::open(&path_a).expect("open db a");
            rollup(&db, "X", "claude", "2026-04-24").await;
        });
    });
    let handle_b = thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        rt.block_on(async move {
            let db = Database::open(&path_b).expect("open db b");
            rollup(&db, "Y", "claude", "2026-04-24").await;
        });
    });

    handle_a.join().expect("thread a");
    handle_b.join().expect("thread b");

    let db = Database::open(&db_path).expect("reopen db");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
    let drawer = db
        .get_drawer(&diary_rollup_drawer_id("claude", "2026-04-24"))
        .expect("get drawer")
        .expect("drawer exists");
    assert!(drawer.content.contains('X'));
    assert!(drawer.content.contains('Y'));
}

#[tokio::test]
async fn test_file_ingest_diary_rollup_uses_options() {
    let (tmp, db) = new_db();
    let source = tmp.path().join("entry.md");
    std::fs::write(&source, "OBSERVATION: file entry").expect("write source");

    let stats = ingest_file_with_options(
        &db,
        &LengthEmbedder,
        &source,
        DIARY_ROLLUP_WING,
        IngestOptions {
            room: Some("claude"),
            source_root: source.parent(),
            dry_run: false,
            diary_rollup: true,
            diary_rollup_day: Some("2026-04-24"),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("file rollup ingest");

    assert_eq!(stats.files, 1);
    assert_eq!(stats.chunks, 1);
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
}
