use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

#[derive(Clone)]
struct BarrierFailingEmbedderFactory {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

struct BarrierFailingEmbedder {
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl EmbedderFactory for BarrierFailingEmbedderFactory {
    async fn build(&self) -> Result<Box<dyn Embedder>, EmbedError> {
        Ok(Box::new(BarrierFailingEmbedder {
            entered: Arc::clone(&self.entered),
            release: Arc::clone(&self.release),
        }))
    }
}

#[async_trait]
impl Embedder for BarrierFailingEmbedder {
    async fn embed(&self, _texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.entered.notify_one();
        self.release.notified().await;
        Err(EmbedError::Runtime(
            "synthetic barrier embedder failure".to_string(),
        ))
    }

    fn dimensions(&self) -> usize {
        4
    }

    fn name(&self) -> &str {
        "barrier-failing-search-budget-test"
    }
}

#[tokio::test]
async fn unrepresentable_caller_deadline_returns_structured_admission_error() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::with_options(true, 600, 600, "");
    let state = env.state(Arc::new(FailingEmbedderFactory));

    let (status, _headers, body) = get_json(
        state,
        "/api/search?q=alpha&scope=global&top_k=5&deadline_ms=18446744073709551615&correlation_id=overflow-caller",
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["status"], 400);
    assert_eq!(body["error"]["kind"], "search_admission_error");
    assert_eq!(body["error"]["retryable"], false);
    let rendered = body.to_string();
    assert!(rendered.contains("deadline_ms"), "body={body:#}");
    assert!(rendered.contains("representable"), "body={body:#}");
}

#[tokio::test]
async fn unrepresentable_hot_reload_deadline_preserves_last_valid_config() {
    let _guard = TEST_LOCK.lock().await;
    let env = TestEnv::with_options(true, 600, 600, "");
    let stable_version = ConfigHandle::version();
    let invalid = format!(
        r#"
db_path = "{}"

[config_hot_reload]
enabled = false

[search]
bm25_fallback = true

[embed.retry]
search_deadline_secs = 30

[api]
search_query_deadline_secs = 9223372036854775807
search_db_deadline_secs = 600
"#,
        env.db_path.display()
    );

    env.rewrite_and_reload(&invalid);

    assert_eq!(ConfigHandle::version(), stable_version);
    assert_eq!(ConfigHandle::current().api.search_query_deadline_secs, 600);
    assert!(
        ConfigHandle::recent_events()
            .iter()
            .any(|event| event.contains("search_query_deadline_secs")
                && event.contains("representable")),
        "events={:#?}",
        ConfigHandle::recent_events()
    );
}

#[tokio::test]
async fn reranker_and_remote_policy_are_snapshotted_at_query_admission() {
    let _guard = TEST_LOCK.lock().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind admission-snapshot reranker");
    let addr = listener.local_addr().expect("reranker address");
    let old_reranker = tokio::spawn(async move {
        let accepted = tokio::time::timeout(Duration::from_secs(3), listener.accept()).await;
        let Ok(Ok((mut stream, _))) = accepted else {
            return false;
        };
        let mut buffer = [0_u8; 4096];
        let _ = stream.read(&mut buffer).await;
        let body =
            r#"{"results":[{"index":1,"relevance_score":0.9},{"index":0,"relevance_score":0.1}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write reranker response");
        true
    });
    let env = TestEnv::new(&format!(
        r#"
[privacy.remote_calls]
fail_closed = true
allow_rerank = true

[search.reranker]
enabled = true
endpoint = "http://{addr}/v1/rerank"
model = "admission-old"
timeout_secs = 30
top_k = 50
"#
    ));
    let db = env.db();
    insert_search_drawer(&db, "drawer_snapshot_a", "alpha primary memory", 4);
    insert_search_drawer(&db, "drawer_snapshot_b", "alpha secondary memory", 2);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let state = env.state(Arc::new(BarrierFailingEmbedderFactory {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    }));
    let first_state = state.clone();
    let first = tokio::spawn(async move {
        get_json(
            first_state,
            "/api/search?q=alpha&scope=global&top_k=5&correlation_id=admission-old",
        )
        .await
    });
    entered.notified().await;

    let reloaded = format!(
        r#"
db_path = "{}"

[config_hot_reload]
enabled = false

[search]
bm25_fallback = true

[privacy.remote_calls]
fail_closed = true
allow_rerank = false

[embed.retry]
search_deadline_secs = 30

[api]
search_query_deadline_secs = 30
search_db_deadline_secs = 30

[search.reranker]
enabled = false
endpoint = "http://127.0.0.1:9/v1/rerank"
model = "admission-new"
timeout_secs = 1
top_k = 1
"#,
        env.db_path.display()
    );
    env.rewrite_and_reload(&reloaded);
    release.notify_one();

    let (status, _headers, first_body) = first.await.expect("join first search");
    assert_eq!(status, StatusCode::OK);
    assert!(old_reranker.await.expect("join old reranker"));
    let first_results = first_body.as_array().expect("first result array");
    assert_eq!(first_results[0]["drawer_id"], "drawer_snapshot_b");

    release.notify_one();
    let (status, _headers, second_body) = get_json(
        state,
        "/api/search?q=alpha&scope=global&top_k=5&correlation_id=admission-new",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second_results = second_body.as_array().expect("second result array");
    assert_eq!(second_results[0]["drawer_id"], "drawer_snapshot_a");
}
