pub mod client;
pub mod retry;
pub mod router;
pub mod status;
pub mod worker;

#[cfg(test)]
use std::sync::{Arc, OnceLock};

pub use client::{LlmClient, LlmError, LlmMessage, LlmRequest, LlmResponse, Usage};
pub use retry::retry_llm_operation;
pub use router::{LlmRouter, RoutedLlmResponse};
pub use status::{LlmStatus, LlmWarning};
pub use worker::{DEFAULT_GATING_JUDGE_PROMPT, LlmTaskPayload, process_llm_task};

#[cfg(test)]
pub(crate) fn global_llm_worker_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    // ponytail: process-wide worker-test lock; split by worker fixture if throughput matters.
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}
