use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use arc_swap::{ArcSwap, ArcSwapOption};

use crate::core::config::ConfigHandle;

use super::alerting;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedWarning {
    pub level: &'static str,
    pub message: String,
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedHealthSnapshot {
    pub fail_count: u64,
    pub degraded: bool,
    pub last_error: Option<String>,
    pub last_success_at_unix_ms: Option<u64>,
    pub fallback_warning: Option<String>,
}

pub struct EmbedStatus {
    fail_count: AtomicU64,
    degraded: AtomicBool,
    retry_interval_secs: ArcSwap<u64>,
    alert_threshold: ArcSwap<u64>,
    degrade_threshold: ArcSwap<u64>,
    alert_script: ArcSwapOption<PathBuf>,
    block_writes: ArcSwap<bool>,
    alert_enabled: ArcSwap<bool>,
    last_error: ArcSwapOption<String>,
    last_success_at_unix_ms: AtomicU64,
    fallback_warning: ArcSwapOption<String>,
}

impl Default for EmbedStatus {
    fn default() -> Self {
        Self::new()
    }
}

impl EmbedStatus {
    pub fn global() -> &'static Self {
        static INSTANCE: OnceLock<EmbedStatus> = OnceLock::new();
        INSTANCE.get_or_init(Self::new)
    }

    pub fn new() -> Self {
        Self {
            fail_count: AtomicU64::new(0),
            degraded: AtomicBool::new(false),
            retry_interval_secs: ArcSwap::from_pointee(2),
            alert_threshold: ArcSwap::from_pointee(100),
            degrade_threshold: ArcSwap::from_pointee(10),
            alert_script: ArcSwapOption::from(None),
            block_writes: ArcSwap::from_pointee(true),
            alert_enabled: ArcSwap::from_pointee(false),
            last_error: ArcSwapOption::from(None),
            last_success_at_unix_ms: AtomicU64::new(0),
            fallback_warning: ArcSwapOption::from(None),
        }
    }

    pub fn sync_from_config(&self) {
        let config = ConfigHandle::current();
        self.retry_interval_secs
            .store(std::sync::Arc::new(config.embed.retry.interval_secs));
        self.alert_threshold.store(std::sync::Arc::new(
            config.embed.alert.alert_every_n_failures,
        ));
        self.degrade_threshold.store(std::sync::Arc::new(
            config.embed.degradation.degrade_after_n_failures,
        ));
        self.block_writes.store(std::sync::Arc::new(
            config.embed.degradation.block_writes_when_degraded,
        ));
        self.alert_enabled
            .store(std::sync::Arc::new(config.embed.alert.enabled));
        let alert_script = config
            .embed
            .alert
            .script_path
            .as_deref()
            .filter(|path| !path.trim().is_empty())
            .map(PathBuf::from)
            .map(std::sync::Arc::new);
        self.alert_script.store(alert_script);
    }

    pub fn retry_interval_secs(&self) -> u64 {
        self.sync_from_config();
        **self.retry_interval_secs.load()
    }

    pub fn record_failure(&self, error: &impl std::fmt::Display) {
        self.sync_from_config();
        let message = error.to_string();
        self.last_error
            .store(Some(std::sync::Arc::new(message.clone())));
        let fail_count = self.fail_count.fetch_add(1, Ordering::SeqCst) + 1;
        let degrade_after = **self.degrade_threshold.load();
        if fail_count > degrade_after {
            self.degraded.store(true, Ordering::SeqCst);
        }
        self.maybe_fire_alert(fail_count, &message);
    }

    pub fn record_primary_success(&self) {
        self.sync_from_config();
        self.fail_count.store(0, Ordering::SeqCst);
        self.degraded.store(false, Ordering::SeqCst);
        self.last_error.store(None);
        self.fallback_warning.store(None);
        self.last_success_at_unix_ms
            .store(now_unix_ms(), Ordering::SeqCst);
    }

    pub fn record_fallback_success(&self, message: String) {
        self.sync_from_config();
        self.fail_count.store(0, Ordering::SeqCst);
        self.degraded.store(false, Ordering::SeqCst);
        self.last_error.store(None);
        self.fallback_warning
            .store(Some(std::sync::Arc::new(message)));
        self.last_success_at_unix_ms
            .store(now_unix_ms(), Ordering::SeqCst);
    }

    pub fn should_block_writes(&self) -> bool {
        self.sync_from_config();
        self.degraded.load(Ordering::SeqCst) && **self.block_writes.load()
    }

    pub fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    pub fn snapshot(&self) -> EmbedHealthSnapshot {
        self.sync_from_config();
        EmbedHealthSnapshot {
            fail_count: self.fail_count.load(Ordering::SeqCst),
            degraded: self.degraded.load(Ordering::SeqCst),
            last_error: self.last_error.load_full().as_deref().cloned(),
            last_success_at_unix_ms: non_zero(self.last_success_at_unix_ms.load(Ordering::SeqCst)),
            fallback_warning: self.fallback_warning.load_full().as_deref().cloned(),
        }
    }

    pub fn collect_warnings(&self) -> Vec<EmbedWarning> {
        let snapshot = self.snapshot();
        let mut warnings = Vec::new();
        if snapshot.degraded {
            warnings.push(EmbedWarning {
                level: "error",
                message: format!(
                    "embed backend degraded after {} failures; writes may be paused until recovery",
                    snapshot.fail_count
                ),
                source: "embed",
            });
        }
        if let Some(message) = snapshot.fallback_warning {
            warnings.push(EmbedWarning {
                level: "warn",
                message,
                source: "embed",
            });
        }
        warnings
    }

    pub fn reset_for_tests(&self) {
        self.fail_count.store(0, Ordering::SeqCst);
        self.degraded.store(false, Ordering::SeqCst);
        self.last_error.store(None);
        self.last_success_at_unix_ms.store(0, Ordering::SeqCst);
        self.fallback_warning.store(None);
    }

    fn maybe_fire_alert(&self, fail_count: u64, error_message: &str) {
        if !**self.alert_enabled.load() {
            return;
        }
        let threshold = **self.alert_threshold.load();
        if threshold == 0 || fail_count % threshold != 0 {
            return;
        }
        let Some(path) = self.alert_script.load_full().as_deref().cloned() else {
            return;
        };
        alerting::fire_alert(&path, fail_count, error_message);
    }
}

pub fn global_embed_status() -> &'static EmbedStatus {
    EmbedStatus::global()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn non_zero(value: u64) -> Option<u64> {
    (value != 0).then_some(value)
}
