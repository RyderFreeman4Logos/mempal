use std::fs;
use std::path::Path;

use mempal_core::db::Database;
use mempal_embed::Embedder;
use mempal_ingest::{ingest_dir, ingest_file};
use tempfile::tempdir;

#[derive(Default)]
struct TestEmbedder;

#[async_trait::async_trait]
impl Embedder for TestEmbedder {
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|text| fake_embedding(text)).collect())
    }

    fn dimensions(&self) -> usize {
        384
    }

    fn name(&self) -> &str {
        "test"
    }
}

fn fake_embedding(text: &str) -> Vec<f32> {
    let mut embedding = vec![0.0_f32; 384];
    for (index, byte) in text.bytes().enumerate() {
        embedding[index % 384] += f32::from(byte) / 255.0;
    }
    embedding
}

fn write_file(path: &Path, content: &str) {
    fs::write(path, content).expect("test fixture should be written");
}

#[tokio::test]
async fn test_ingest_text_file() {
    let dir = tempdir().expect("temp dir should be created");
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).expect("database should open");
    let embedder = TestEmbedder;

    let file = dir.path().join("readme.md");
    write_file(
        &file,
        "We decided to use PostgreSQL for the analytics database.",
    );

    let stats = ingest_file(&db, &embedder, &file, "myproject", None)
        .await
        .expect("file ingest should succeed");
    assert!(stats.chunks > 0);

    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE wing = 'myproject'",
            [],
            |row| row.get(0),
        )
        .expect("drawer count query should succeed");
    assert!(count > 0);

    let vector_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawer_vectors", [], |row| row.get(0))
        .expect("vector count query should succeed");
    assert_eq!(vector_count, count);
}

#[tokio::test]
async fn test_ingest_dedup() {
    let dir = tempdir().expect("temp dir should be created");
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).expect("database should open");
    let embedder = TestEmbedder;

    let file = dir.path().join("notes.md");
    write_file(
        &file,
        "A stable ingest ID should deduplicate repeated imports.",
    );

    ingest_file(&db, &embedder, &file, "myproject", None)
        .await
        .expect("first ingest should succeed");
    let second = ingest_file(&db, &embedder, &file, "myproject", None)
        .await
        .expect("second ingest should succeed");

    assert_eq!(second.chunks, 0);

    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
        .expect("drawer count query should succeed");
    assert_eq!(count, 1);
}

#[tokio::test]
async fn test_ingest_empty_file() {
    let dir = tempdir().expect("temp dir should be created");
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).expect("database should open");
    let embedder = TestEmbedder;

    let file = dir.path().join("empty.md");
    write_file(&file, "");

    let stats = ingest_file(&db, &embedder, &file, "myproject", None)
        .await
        .expect("empty file ingest should not error");

    assert_eq!(stats.chunks, 0);

    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
        .expect("drawer count query should succeed");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn test_ingest_directory() {
    let dir = tempdir().expect("temp dir should be created");
    let db_path = dir.path().join("test.db");
    let db = Database::open(&db_path).expect("database should open");
    let embedder = TestEmbedder;

    let project_dir = dir.path().join("project");
    let src_dir = project_dir.join("src");
    let nested_dir = src_dir.join("nested");
    fs::create_dir_all(&nested_dir).expect("source directories should be created");
    fs::create_dir_all(project_dir.join(".git")).expect("ignored directory should be created");
    fs::create_dir_all(project_dir.join("target")).expect("ignored directory should be created");

    write_file(&src_dir.join("lib.rs"), "pub fn alpha() {}");
    write_file(&src_dir.join("main.rs"), "fn main() {}");
    write_file(&nested_dir.join("util.rs"), "pub fn beta() {}");
    write_file(&project_dir.join("README.md"), "Project notes live here.");
    write_file(&project_dir.join(".git").join("ignored.txt"), "ignore me");

    let stats = ingest_dir(&db, &embedder, &project_dir, "myproject", None)
        .await
        .expect("directory ingest should succeed");

    assert_eq!(stats.files, 4);

    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
        .expect("drawer count query should succeed");
    assert!(count >= 4);
}
