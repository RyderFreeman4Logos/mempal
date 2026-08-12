use std::sync::PoisonError;

use super::*;

impl ConnPool {
    /// Pre-open `count` connections at `path`. Readers (`query_only == true`)
    /// are opened read-write-capable then flipped to `PRAGMA query_only=ON`.
    pub(super) fn open(path: &Path, count: usize, query_only: bool) -> Result<Self, DbError> {
        let mut idle = Vec::with_capacity(count);
        for _ in 0..count {
            idle.push(Self::open_one(path, query_only)?);
        }
        Ok(Self {
            sem: Arc::new(Semaphore::new(count)),
            idle: Mutex::new(idle),
            path: path.to_path_buf(),
            query_only,
            count,
            #[cfg(any(test, feature = "db-test-seam"))]
            reopen_delay: Mutex::new(None),
            #[cfg(any(test, feature = "db-test-seam"))]
            write_busy_timeout: Mutex::new(None),
            #[cfg(any(test, feature = "db-test-seam"))]
            write_busy_events: Mutex::new(None),
        })
    }

    fn open_one(path: &Path, query_only: bool) -> Result<Database, DbError> {
        if query_only {
            Database::open_query_only_unadmitted(path)
        } else {
            Database::open_unadmitted(path)
        }
    }

    fn open_one_with_busy_timeout(
        path: &Path,
        query_only: bool,
        busy_timeout: Duration,
    ) -> Result<Database, DbError> {
        if query_only {
            Database::open_query_only_unadmitted_with_busy_timeout(path, busy_timeout)
        } else {
            Database::open_unadmitted_with_busy_timeout(path, busy_timeout)
        }
    }

    /// Pop an idle connection; reopen a fresh one if the pool was transiently
    /// drained by a cancelled checkout (self-heal — the pool never shrinks).
    pub(super) fn take_or_open(&self) -> Result<Database, DbError> {
        let popped = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        match popped {
            Some(db) => Ok(db),
            None => {
                self.wait_before_reopen(None)?;
                Self::open_one(&self.path, self.query_only)
            }
        }
    }

    /// Take an idle connection or self-heal it within the same mutation budget.
    /// Callers must invoke this off the async runtime.
    pub(super) fn take_or_open_until(&self, deadline: Instant) -> Result<Database, DbError> {
        let popped = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        match popped {
            Some(db) => Ok(db),
            None => {
                self.wait_before_reopen(Some(deadline))?;
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(write_deadline_exceeded_error());
                }
                Self::open_one_with_busy_timeout(&self.path, self.query_only, remaining)
            }
        }
    }

    fn wait_before_reopen(&self, deadline: Option<Instant>) -> Result<(), DbError> {
        #[cfg(any(test, feature = "db-test-seam"))]
        if let Some(delay) = *self
            .reopen_delay
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            let sleep_for = deadline.map_or(delay, |deadline| {
                delay.min(deadline.saturating_duration_since(Instant::now()))
            });
            if sleep_for.is_zero() {
                return Err(write_deadline_exceeded_error());
            }
            std::thread::sleep(sleep_for);
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(write_deadline_exceeded_error());
            }
        }
        #[cfg(not(any(test, feature = "db-test-seam")))]
        let _ = deadline;
        Ok(())
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub(super) fn set_reopen_delay(&self, delay: Duration) {
        *self
            .reopen_delay
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(delay);
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub(super) fn set_write_busy_timeout_and_events(
        &self,
        busy_timeout: Duration,
        busy_events: tokio::sync::mpsc::UnboundedSender<()>,
    ) {
        *self
            .write_busy_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(busy_timeout);
        *self
            .write_busy_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(busy_events);
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub(super) fn write_busy_timeout(&self) -> Option<Duration> {
        *self
            .write_busy_timeout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub(super) fn report_busy_write_attempt<R>(&self, result: &Result<R, DbError>) {
        if !result
            .as_ref()
            .err()
            .is_some_and(crate::core::db::db_error_is_sqlite_lock)
        {
            return;
        }
        if let Some(busy_events) = self
            .write_busy_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            let _ = busy_events.send(());
        }
    }

    pub(super) fn checkin(&self, db: Database) {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(db);
    }
}
