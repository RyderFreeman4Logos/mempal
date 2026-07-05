use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::core::{
    AsyncDb,
    async_db::{
        AsyncDbResourceSnapshot, RESOURCE_BOUNDED_READERS, anyhow_error_is_read_deadline_exceeded,
    },
    config::ConfigHandle,
    db::DbError,
};
use crate::embed::EmbedderFactory;
use serde::Serialize;
use tokio::sync::OnceCell;

const API_ASYNC_DB_READERS: usize = RESOURCE_BOUNDED_READERS;
const MAX_SLOW_SEARCHES: usize = 10;
const SLOW_SEARCH_THRESHOLD: Duration = Duration::from_secs(1);
const SQLITE_INTERRUPT_GRACE: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct ApiState {
    pub db_path: PathBuf,
    pub embedder_factory: Arc<dyn EmbedderFactory>,
    async_db: Arc<OnceCell<AsyncDb>>,
    write_queue: Arc<super::handlers::WriteQueue>,
    search_telemetry: Arc<SearchTelemetry>,
    bounded_read_counter: Option<Arc<AtomicU64>>,
}

impl ApiState {
    pub fn new(db_path: PathBuf, embedder_factory: Arc<dyn EmbedderFactory>) -> Self {
        let config = ConfigHandle::current();
        Self::with_write_queue_config(
            db_path,
            embedder_factory,
            config.api.write_queue_capacity,
            Duration::from_secs(config.api.write_drain_timeout_secs),
        )
    }

    pub fn with_write_queue_config(
        db_path: PathBuf,
        embedder_factory: Arc<dyn EmbedderFactory>,
        queue_capacity: usize,
        drain_timeout: Duration,
    ) -> Self {
        let write_queue = Arc::new(super::handlers::WriteQueue::spawn(
            db_path.clone(),
            Arc::clone(&embedder_factory),
            queue_capacity,
            drain_timeout,
        ));
        Self {
            db_path,
            embedder_factory,
            async_db: Arc::new(OnceCell::new()),
            write_queue,
            search_telemetry: Arc::new(SearchTelemetry::default()),
            bounded_read_counter: None,
        }
    }

    pub(crate) fn write_queue(&self) -> &super::handlers::WriteQueue {
        &self.write_queue
    }

    pub(crate) async fn async_db(&self) -> Result<AsyncDb, DbError> {
        let db_path = self.db_path.clone();
        self.async_db
            .get_or_try_init(|| async move { AsyncDb::open(&db_path, API_ASYNC_DB_READERS) })
            .await
            .cloned()
    }

    pub(crate) fn async_db_resource_snapshot(&self) -> Option<AsyncDbResourceSnapshot> {
        self.async_db.get().map(AsyncDb::resource_snapshot)
    }

    pub(crate) async fn run_read_anyhow_bounded<F, R>(
        &self,
        f: F,
        deadline: Duration,
    ) -> anyhow::Result<Option<R>>
    where
        F: FnOnce(&crate::core::db::Database) -> anyhow::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        if let Some(counter) = &self.bounded_read_counter {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        let async_db = self.async_db().await?;
        let sqlite_deadline = Instant::now() + deadline;
        match tokio::time::timeout(
            deadline + SQLITE_INTERRUPT_GRACE,
            async_db.run_read_anyhow_until(sqlite_deadline, f),
        )
        .await
        {
            Ok(Ok(result)) => Ok(Some(result)),
            Ok(Err(error)) if anyhow_error_is_read_deadline_exceeded(&error) => Ok(None),
            Ok(Err(error)) => Err(error),
            Err(_) => Ok(None),
        }
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn with_async_db_for_test(mut self, async_db: AsyncDb) -> Self {
        let cell = Arc::new(OnceCell::new());
        debug_assert!(cell.set(async_db).is_ok());
        self.async_db = cell;
        self
    }

    #[doc(hidden)]
    pub fn with_bounded_read_counter_for_test(mut self, counter: Arc<AtomicU64>) -> Self {
        self.bounded_read_counter = Some(counter);
        self
    }

    pub(crate) fn search_telemetry(&self) -> &Arc<SearchTelemetry> {
        &self.search_telemetry
    }

    #[cfg(any(test, feature = "db-test-seam"))]
    pub fn search_telemetry_snapshot_for_test(&self) -> SearchTelemetrySnapshot {
        self.search_telemetry.snapshot()
    }

    pub async fn drain_write_queue(&self) -> bool {
        self.write_queue.drain().await
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchTelemetrySnapshot {
    pub active_count: usize,
    pub active_searches: Vec<ActiveSearchSnapshot>,
    pub slow_queries: Vec<SlowSearchSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveSearchSnapshot {
    pub id: u64,
    pub client: String,
    pub scope: String,
    pub top_k: usize,
    pub stage: String,
    pub started_at_unix_ms: u64,
    pub elapsed_ms: u64,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SlowSearchSnapshot {
    pub id: u64,
    pub client: String,
    pub scope: String,
    pub top_k: usize,
    pub search_mode: String,
    pub elapsed_ms: u64,
    pub deadline_ms: u64,
    pub route_ms: u64,
    pub embed_ms: u64,
    pub db_ms: u64,
    pub rerank_ms: u64,
    pub lock_wait_ms: u64,
    pub result_count: usize,
    pub warning_count: usize,
    pub partial: bool,
    pub completed_at_unix_ms: u64,
}

#[derive(Debug, Clone)]
struct ActiveSearch {
    id: u64,
    client: String,
    scope: String,
    top_k: usize,
    stage: String,
    started_at_unix_ms: u64,
    started_at: Instant,
    deadline_ms: u64,
}

#[derive(Default)]
pub(crate) struct SearchTelemetry {
    next_id: AtomicU64,
    active: Mutex<BTreeMap<u64, ActiveSearch>>,
    slow_queries: Mutex<VecDeque<SlowSearchSnapshot>>,
}

impl SearchTelemetry {
    pub(crate) fn start(
        self: &Arc<Self>,
        client: String,
        scope: String,
        top_k: usize,
        deadline: Duration,
    ) -> SearchTelemetryGuard {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let search = ActiveSearch {
            id,
            client,
            scope,
            top_k,
            stage: "queued".to_string(),
            started_at_unix_ms: unix_ms_now(),
            started_at: Instant::now(),
            deadline_ms: duration_ms(deadline),
        };
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id, search);
        SearchTelemetryGuard {
            telemetry: Arc::clone(self),
            id,
            finished: false,
        }
    }

    fn set_stage(&self, id: u64, stage: &str) {
        if let Some(search) = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&id)
        {
            search.stage = stage.to_string();
        }
    }

    fn finish(&self, id: u64, outcome: SearchTelemetryOutcome) {
        let Some(active) = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id)
        else {
            return;
        };
        let elapsed_ms = duration_ms(active.started_at.elapsed());
        if active.started_at.elapsed() < SLOW_SEARCH_THRESHOLD && !outcome.partial {
            return;
        }
        let snapshot = SlowSearchSnapshot {
            id: active.id,
            client: active.client,
            scope: active.scope,
            top_k: active.top_k,
            search_mode: outcome.search_mode,
            elapsed_ms,
            deadline_ms: active.deadline_ms,
            route_ms: duration_ms(outcome.route),
            embed_ms: duration_ms(outcome.embed),
            db_ms: duration_ms(outcome.db),
            rerank_ms: duration_ms(outcome.rerank),
            lock_wait_ms: duration_ms(outcome.lock_wait),
            result_count: outcome.result_count,
            warning_count: outcome.warning_count,
            partial: outcome.partial,
            completed_at_unix_ms: unix_ms_now(),
        };
        let mut slow_queries = self
            .slow_queries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slow_queries.push_front(snapshot);
        slow_queries.truncate(MAX_SLOW_SEARCHES);
    }

    fn remove_active(&self, id: u64) {
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&id);
    }

    pub(crate) fn snapshot(&self) -> SearchTelemetrySnapshot {
        let active_searches = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|search| ActiveSearchSnapshot {
                id: search.id,
                client: search.client.clone(),
                scope: search.scope.clone(),
                top_k: search.top_k,
                stage: search.stage.clone(),
                started_at_unix_ms: search.started_at_unix_ms,
                elapsed_ms: duration_ms(search.started_at.elapsed()),
                deadline_ms: search.deadline_ms,
            })
            .collect::<Vec<_>>();
        let slow_queries = self
            .slow_queries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        SearchTelemetrySnapshot {
            active_count: active_searches.len(),
            active_searches,
            slow_queries,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SearchTelemetryOutcome {
    pub search_mode: String,
    pub route: Duration,
    pub embed: Duration,
    pub db: Duration,
    pub rerank: Duration,
    pub lock_wait: Duration,
    pub result_count: usize,
    pub warning_count: usize,
    pub partial: bool,
}

pub(crate) struct SearchTelemetryGuard {
    telemetry: Arc<SearchTelemetry>,
    id: u64,
    finished: bool,
}

impl SearchTelemetryGuard {
    pub(crate) fn set_stage(&self, stage: &str) {
        self.telemetry.set_stage(self.id, stage);
    }

    pub(crate) fn finish(mut self, outcome: SearchTelemetryOutcome) {
        self.telemetry.finish(self.id, outcome);
        self.finished = true;
    }
}

impl Drop for SearchTelemetryGuard {
    fn drop(&mut self) {
        if !self.finished {
            self.telemetry.remove_active(self.id);
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(duration_ms)
        .unwrap_or(0)
}
