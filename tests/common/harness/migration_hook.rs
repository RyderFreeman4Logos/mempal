//! Test doubles for the fork-ext migration hook.

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use mempal::core::db::MigrationHook;

pub struct NoopMigrationHook;

impl MigrationHook for NoopMigrationHook {
    fn pre_commit(&self) -> Result<()> {
        Ok(())
    }
}

pub struct AlwaysFailMigrationHook;

impl MigrationHook for AlwaysFailMigrationHook {
    fn pre_commit(&self) -> Result<()> {
        Err(anyhow!("simulated crash"))
    }
}

pub struct CountingMigrationHook {
    count: AtomicUsize,
}

impl CountingMigrationHook {
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
        }
    }

    pub fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl MigrationHook for CountingMigrationHook {
    fn pre_commit(&self) -> Result<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_noop_hook() {
        let hook = NoopMigrationHook;
        assert!(hook.pre_commit().is_ok());
    }

    #[test]
    fn smoke_failing_hook() {
        let hook = AlwaysFailMigrationHook;
        assert!(hook.pre_commit().is_err());
    }
}
