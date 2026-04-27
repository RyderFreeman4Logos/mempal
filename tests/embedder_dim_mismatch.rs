use std::sync::{Arc, OnceLock};

use mempal::core::config::Config;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::embed::global_embed_status;
use mempal::mcp::{IngestRequest, MempalMcpServer};
use mockito::{Matcher, Server};
use rmcp::handler::server::wrapper::Parameters;
use tempfile::TempDir;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_dim_mismatch_detected_before_reindex() {
    let _guard = test_guard().await;
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: "existing".to_string(),
        content: "existing drawer".to_string(),
        wing: "test".to_string(),
        room: Some("room".to_string()),
        source_file: Some("existing.txt".to_string()),
        source_type: SourceType::Project,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance: 0,
        ..Drawer::default()
    })
    .expect("insert drawer");
    db.insert_vector("existing", &[0.9, 0.8])
        .expect("insert 2d vector");

    let mut server = Server::new_async().await;
    let _mock = server
        .mock("POST", "/v1/embeddings")
        .match_body(Matcher::Any)
        .with_status(200)
        .with_body(r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#)
        .create();

    let config = Config::parse(&format!(
        r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}/v1"
model = "Qwen/Qwen3-Embedding-8B"
dim = 3
request_timeout_secs = 5
"#,
        db_path.display(),
        server.url()
    ))
    .expect("parse config");
    global_embed_status().reset_for_tests();

    let service = MempalMcpServer::new(db_path, config);
    let error = match service
        .mempal_ingest(Parameters(IngestRequest {
            content: "new drawer".to_string(),
            wing: "test".to_string(),
            room: Some("room".to_string()),
            dry_run: Some(false),
            ..IngestRequest::default()
        }))
        .await
    {
        Ok(_) => panic!("ingest should fail on dim mismatch"),
        Err(error) => error,
    };

    let rendered = format!("{error:?}");
    assert!(rendered.contains("dimension mismatch"));
    assert!(rendered.contains("mempal reindex"));
    global_embed_status().reset_for_tests();
}
