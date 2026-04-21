//! Shared observation handle for config hot-reload application counts.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mempal::core::config::ConfigHandle;

#[derive(Debug, Clone)]
pub struct ReloadCounter(Arc<AtomicUsize>);

impl ReloadCounter {
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    pub fn from_hot_reload_state() -> Self {
        Self(ConfigHandle::harness_reload_counter())
    }

    pub fn increment(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    pub fn count(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.0.store(0, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_counter_resets() {
        let counter = ReloadCounter::new();
        counter.increment();
        assert_eq!(counter.count(), 1);
        counter.reset();
        assert_eq!(counter.count(), 0);
    }
}
