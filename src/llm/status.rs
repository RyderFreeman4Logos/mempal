use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmWarning {
    pub level: &'static str,
    pub message: String,
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmHealthSnapshot {
    pub fail_count: u64,
    pub degraded: bool,
    pub last_error: Option<String>,
}

pub struct LlmStatus {
    fail_count: AtomicU64,
    degraded: AtomicBool,
    degrade_threshold: u64,
    last_error: std::sync::Mutex<Option<String>>,
}

impl LlmStatus {
    pub fn new(degrade_threshold: u64) -> Self {
        Self {
            fail_count: AtomicU64::new(0),
            degraded: AtomicBool::new(false),
            degrade_threshold,
            last_error: std::sync::Mutex::new(None),
        }
    }

    pub fn record_failure(&self, error: &impl std::fmt::Display) {
        let message = crate::core::config::scrub_sensitive_text(&error.to_string());
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = Some(message);
        }
        let fail_count = self.fail_count.fetch_add(1, Ordering::SeqCst) + 1;
        if fail_count >= self.degrade_threshold {
            self.degraded.store(true, Ordering::SeqCst);
        }
    }

    pub fn record_success(&self) {
        self.fail_count.store(0, Ordering::SeqCst);
        self.degraded.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = None;
        }
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    pub fn should_block_writes(&self) -> bool {
        self.is_degraded()
    }

    pub fn snapshot(&self) -> LlmHealthSnapshot {
        LlmHealthSnapshot {
            fail_count: self.fail_count.load(Ordering::SeqCst),
            degraded: self.degraded.load(Ordering::SeqCst),
            last_error: self.last_error.lock().ok().and_then(|guard| guard.clone()),
        }
    }

    pub fn collect_warnings(&self) -> Vec<LlmWarning> {
        let snapshot = self.snapshot();
        let mut warnings = Vec::new();
        if snapshot.degraded {
            warnings.push(LlmWarning {
                level: "error",
                message: format!(
                    "LLM backend degraded after {} consecutive failures; distill/compress operations paused",
                    snapshot.fail_count
                ),
                source: "llm",
            });
        }
        if let Some(error) = snapshot.last_error {
            if !snapshot.degraded {
                warnings.push(LlmWarning {
                    level: "warn",
                    message: format!("LLM backend last error: {error}"),
                    source: "llm",
                });
            }
        }
        warnings
    }
}
