use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use mempal::core::config::ConfigHandle;
use mempal::embed::{EmbedError, retry::retry_embed_operation, status::EmbedStatus};
use tempfile::TempDir;

async fn test_guard() -> tokio::sync::OwnedMutexGuard<()> {
    static GUARD: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    GUARD
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}

fn bootstrap_retry_config() -> TempDir {
    let tmp = TempDir::new().expect("tempdir");
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
db_path = "/tmp/mempal-test.db"

[embed]
backend = "openai_compat"

[embed.retry]
interval_secs = 1
search_deadline_secs = 5
"#,
    )
    .expect("write config");
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    tmp
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_embedder_calls_heartbeat_per_retry_iteration() {
    let _guard = test_guard().await;
    let _tmp = bootstrap_retry_config();
    let status = EmbedStatus::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let heartbeats = Arc::new(AtomicUsize::new(0));
    let heartbeat_counter = Arc::clone(&heartbeats);

    let result = retry_embed_operation(
        &status,
        Some(&move || {
            heartbeat_counter.fetch_add(1, Ordering::SeqCst);
            Ok::<(), EmbedError>(())
        }),
        {
            let attempts = Arc::clone(&attempts);
            move || {
                let attempts = Arc::clone(&attempts);
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        Err(EmbedError::Runtime(format!("transient failure {attempt}")))
                    } else {
                        Ok(vec![vec![0.1, 0.2, 0.3]])
                    }
                }
            }
        },
    )
    .await
    .expect("retry succeeds");

    assert_eq!(result, vec![vec![0.1, 0.2, 0.3]]);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(heartbeats.load(Ordering::SeqCst), 4);
}
