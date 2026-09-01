use std::sync::{Arc, OnceLock};

use tokio::sync::Mutex;

// ponytail: class lock; split by resource for throughput.
pub async fn guard() -> impl Send {
    static G: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
    G.get_or_init(|| Arc::new(Mutex::new(())))
        .clone()
        .lock_owned()
        .await
}
