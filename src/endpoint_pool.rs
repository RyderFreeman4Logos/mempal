//! Shared model-endpoint scheduler for LLM and embedding providers.
//!
//! Provider-specific routers own request construction and error mapping. This
//! module owns only stable scheduling concerns: priority ordering, per-endpoint
//! cooldown, saturated-endpoint fallback, and aggregate pool capacity.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// Immutable routing metadata shared by all model-backed endpoint pools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EndpointPoolEndpoint {
    id: String,
    model: String,
    priority: i32,
    max_concurrent: usize,
    retry_interval: Duration,
}

impl EndpointPoolEndpoint {
    pub(crate) fn new(
        id: impl Into<String>,
        model: impl Into<String>,
        priority: i32,
        max_concurrent: usize,
        retry_interval: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            priority,
            max_concurrent,
            retry_interval,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    pub(crate) fn retry_interval(&self) -> Duration {
        self.retry_interval
    }
}

#[derive(Debug)]
pub(crate) struct EndpointPoolItem<C> {
    endpoint: EndpointPoolEndpoint,
    client: C,
}

impl<C> EndpointPoolItem<C> {
    pub(crate) fn new(endpoint: EndpointPoolEndpoint, client: C) -> Self {
        Self { endpoint, client }
    }
}

#[derive(Debug)]
pub(crate) struct EndpointPoolEntry<C> {
    endpoint: EndpointPoolEndpoint,
    client: C,
    unavailable_until: Mutex<Option<Instant>>,
}

impl<C> EndpointPoolEntry<C> {
    pub(crate) fn endpoint(&self) -> &EndpointPoolEndpoint {
        &self.endpoint
    }

    pub(crate) fn client(&self) -> &C {
        &self.client
    }
}

const MAX_COOLDOWN_HINT: Duration = Duration::from_secs(60);

#[derive(Debug)]
pub(crate) struct EndpointPool<C> {
    endpoints: Arc<Vec<Arc<EndpointPoolEntry<C>>>>,
}

impl<C> Clone for EndpointPool<C> {
    fn clone(&self) -> Self {
        Self {
            endpoints: Arc::clone(&self.endpoints),
        }
    }
}

impl<C> EndpointPool<C>
where
    C: Send + Sync,
{
    pub(crate) fn new(items: Vec<EndpointPoolItem<C>>) -> Self {
        let mut items = items.into_iter().enumerate().collect::<Vec<_>>();
        items.sort_by_key(|(index, item)| (item.endpoint.priority, *index));
        let endpoints = items
            .into_iter()
            .map(|(_, item)| {
                Arc::new(EndpointPoolEntry {
                    endpoint: item.endpoint,
                    client: item.client,
                    unavailable_until: Mutex::new(None),
                })
            })
            .collect();
        Self {
            endpoints: Arc::new(endpoints),
        }
    }

    pub(crate) async fn route<S>(&self, strategy: &S) -> Result<S::Output, S::Error>
    where
        S: EndpointPoolStrategy<C>,
    {
        let mut last_retryable = None;
        let mut earliest_retry_after: Option<Duration> = None;
        let mut first_saturated_endpoint: Option<Arc<EndpointPoolEntry<C>>> = None;

        for endpoint in self.endpoints.iter() {
            if let Some(retry_after) = endpoint.temporary_unavailable_remaining(strategy).await {
                earliest_retry_after = Some(min_retry_after(earliest_retry_after, retry_after));
                continue;
            }

            match strategy.try_endpoint(endpoint).await {
                Ok(Some(output)) => {
                    if strategy.clear_cooldown_on_success() {
                        endpoint.mark_available(strategy).await;
                    }
                    return Ok(output);
                }
                Ok(None) => {
                    if first_saturated_endpoint.is_none() {
                        first_saturated_endpoint = Some(Arc::clone(endpoint));
                    }
                }
                Err(error) if strategy.should_try_next(&error) => {
                    if let Some(retry_after) = strategy.retry_after_for_error(endpoint, &error) {
                        endpoint
                            .mark_temporarily_unavailable(strategy, retry_after, &error)
                            .await;
                        earliest_retry_after =
                            Some(min_retry_after(earliest_retry_after, retry_after));
                    }
                    last_retryable = Some(error);
                }
                Err(error) => return Err(error),
            }
        }

        if let Some(endpoint) = first_saturated_endpoint {
            let output = strategy.wait_for_endpoint(&endpoint).await?;
            if strategy.clear_cooldown_on_success() {
                endpoint.mark_available(strategy).await;
            }
            return Ok(output);
        }

        match (last_retryable, earliest_retry_after) {
            (None, Some(retry_after)) | (Some(_), Some(retry_after)) => {
                Err(strategy.all_cooling_down_error(self.endpoints.len(), retry_after))
            }
            (Some(error), None) => Err(error),
            (None, None) => Err(strategy.no_endpoint_available_error()),
        }
    }

    pub(crate) fn endpoint_count(&self) -> usize {
        self.endpoints.len()
    }

    pub(crate) fn pool_capacity(&self) -> usize {
        self.endpoints
            .iter()
            .map(|endpoint| endpoint.endpoint.max_concurrent.max(1))
            .sum()
    }
}

impl<C> EndpointPoolEntry<C> {
    async fn temporary_unavailable_remaining<S>(&self, strategy: &S) -> Option<Duration>
    where
        C: Send + Sync,
        S: EndpointPoolStrategy<C>,
    {
        let mut guard = self.unavailable_until.lock().await;
        match *guard {
            Some(until) if until > Instant::now() => {
                Some(until.saturating_duration_since(Instant::now()))
            }
            Some(_) => {
                *guard = None;
                strategy.on_cooldown_cleared(self);
                None
            }
            None => None,
        }
    }

    async fn mark_temporarily_unavailable<S>(
        &self,
        strategy: &S,
        retry_after: Duration,
        error: &S::Error,
    ) where
        C: Send + Sync,
        S: EndpointPoolStrategy<C>,
    {
        let mut guard = self.unavailable_until.lock().await;
        let now = Instant::now();
        *guard = Some(
            now.checked_add(retry_after)
                .unwrap_or_else(|| now + MAX_COOLDOWN_HINT),
        );
        strategy.on_cooldown_marked(self, retry_after, error);
    }

    async fn mark_available<S>(&self, strategy: &S)
    where
        C: Send + Sync,
        S: EndpointPoolStrategy<C>,
    {
        let mut guard = self.unavailable_until.lock().await;
        *guard = None;
        strategy.on_cooldown_cleared(self);
    }
}

#[async_trait::async_trait]
pub(crate) trait EndpointPoolStrategy<C>: Sync
where
    C: Send + Sync,
{
    type Output: Send;
    type Error: Send;

    async fn try_endpoint(
        &self,
        endpoint: &EndpointPoolEntry<C>,
    ) -> Result<Option<Self::Output>, Self::Error>;

    async fn wait_for_endpoint(
        &self,
        endpoint: &EndpointPoolEntry<C>,
    ) -> Result<Self::Output, Self::Error>;

    fn should_try_next(&self, error: &Self::Error) -> bool;

    fn retry_after_for_error(
        &self,
        endpoint: &EndpointPoolEntry<C>,
        error: &Self::Error,
    ) -> Option<Duration>;

    fn all_cooling_down_error(&self, endpoint_count: usize, retry_after: Duration) -> Self::Error;

    fn no_endpoint_available_error(&self) -> Self::Error;

    fn clear_cooldown_on_success(&self) -> bool {
        false
    }

    fn on_cooldown_marked(
        &self,
        _endpoint: &EndpointPoolEntry<C>,
        _retry_after: Duration,
        _error: &Self::Error,
    ) {
    }

    fn on_cooldown_cleared(&self, _endpoint: &EndpointPoolEntry<C>) {}
}

fn min_retry_after(current: Option<Duration>, next: Duration) -> Duration {
    match current {
        Some(current) => current.min(next),
        None => next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[derive(Debug)]
    struct ScriptedClient {
        id: &'static str,
        attempts: Mutex<VecDeque<Attempt>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    #[derive(Debug)]
    enum Attempt {
        Saturated,
        Retryable(Duration),
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum ScriptError {
        Retryable(Duration),
        CoolingDown(Duration),
        Missing,
    }

    struct ScriptedStrategy;

    #[async_trait::async_trait]
    impl EndpointPoolStrategy<ScriptedClient> for ScriptedStrategy {
        type Output = String;
        type Error = ScriptError;

        async fn try_endpoint(
            &self,
            endpoint: &EndpointPoolEntry<ScriptedClient>,
        ) -> Result<Option<Self::Output>, Self::Error> {
            endpoint
                .client()
                .calls
                .lock()
                .expect("calls")
                .push(format!("try:{}", endpoint.endpoint().id()));
            match endpoint
                .client()
                .attempts
                .lock()
                .expect("attempts")
                .pop_front()
                .expect("scripted attempt")
            {
                Attempt::Saturated => Ok(None),
                Attempt::Retryable(retry_after) => Err(ScriptError::Retryable(retry_after)),
            }
        }

        async fn wait_for_endpoint(
            &self,
            endpoint: &EndpointPoolEntry<ScriptedClient>,
        ) -> Result<Self::Output, Self::Error> {
            endpoint
                .client()
                .calls
                .lock()
                .expect("calls")
                .push(format!("wait:{}", endpoint.endpoint().id()));
            Ok(endpoint.client().id.to_string())
        }

        fn should_try_next(&self, error: &Self::Error) -> bool {
            matches!(error, ScriptError::Retryable(_))
        }

        fn retry_after_for_error(
            &self,
            _endpoint: &EndpointPoolEntry<ScriptedClient>,
            error: &Self::Error,
        ) -> Option<Duration> {
            match error {
                ScriptError::Retryable(retry_after) => Some(*retry_after),
                ScriptError::CoolingDown(_) | ScriptError::Missing => None,
            }
        }

        fn all_cooling_down_error(
            &self,
            _endpoint_count: usize,
            retry_after: Duration,
        ) -> Self::Error {
            ScriptError::CoolingDown(retry_after)
        }

        fn no_endpoint_available_error(&self) -> Self::Error {
            ScriptError::Missing
        }
    }

    #[tokio::test]
    async fn route_waits_for_saturated_higher_priority_before_returning_cooldown() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let pool = EndpointPool::new(vec![
            EndpointPoolItem::new(
                EndpointPoolEndpoint::new("primary", "primary-model", 0, 1, Duration::from_secs(2)),
                ScriptedClient {
                    id: "primary",
                    attempts: Mutex::new(VecDeque::from([Attempt::Saturated])),
                    calls: Arc::clone(&calls),
                },
            ),
            EndpointPoolItem::new(
                EndpointPoolEndpoint::new(
                    "cooldown",
                    "cooldown-model",
                    10,
                    1,
                    Duration::from_secs(60),
                ),
                ScriptedClient {
                    id: "cooldown",
                    attempts: Mutex::new(VecDeque::from([Attempt::Retryable(
                        Duration::from_secs(60),
                    )])),
                    calls: Arc::clone(&calls),
                },
            ),
        ]);

        let routed = pool.route(&ScriptedStrategy).await.expect("routed output");

        assert_eq!(routed, "primary");
        assert_eq!(
            calls.lock().expect("calls").as_slice(),
            ["try:primary", "try:cooldown", "wait:primary"]
        );
        assert_eq!(pool.endpoint_count(), 2);
        assert_eq!(pool.pool_capacity(), 2);
    }

    struct ConcurrentSuccessAfterFailureStrategy {
        calls: AtomicUsize,
        first_started: Notify,
        release_first: Notify,
    }

    #[async_trait::async_trait]
    impl EndpointPoolStrategy<ScriptedClient> for ConcurrentSuccessAfterFailureStrategy {
        type Output = String;
        type Error = ScriptError;

        async fn try_endpoint(
            &self,
            _endpoint: &EndpointPoolEntry<ScriptedClient>,
        ) -> Result<Option<Self::Output>, Self::Error> {
            match self.calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    self.first_started.notify_waiters();
                    self.release_first.notified().await;
                    Ok(Some("success".to_string()))
                }
                1 => Err(ScriptError::Retryable(Duration::from_secs(60))),
                _ => Ok(Some("unexpected-success-after-cooldown".to_string())),
            }
        }

        async fn wait_for_endpoint(
            &self,
            _endpoint: &EndpointPoolEntry<ScriptedClient>,
        ) -> Result<Self::Output, Self::Error> {
            Ok("wait".to_string())
        }

        fn should_try_next(&self, error: &Self::Error) -> bool {
            matches!(error, ScriptError::Retryable(_))
        }

        fn retry_after_for_error(
            &self,
            _endpoint: &EndpointPoolEntry<ScriptedClient>,
            error: &Self::Error,
        ) -> Option<Duration> {
            match error {
                ScriptError::Retryable(retry_after) => Some(*retry_after),
                ScriptError::CoolingDown(_) | ScriptError::Missing => None,
            }
        }

        fn all_cooling_down_error(
            &self,
            _endpoint_count: usize,
            retry_after: Duration,
        ) -> Self::Error {
            ScriptError::CoolingDown(retry_after)
        }

        fn no_endpoint_available_error(&self) -> Self::Error {
            ScriptError::Missing
        }
    }

    #[tokio::test]
    async fn concurrent_success_does_not_clear_newer_cooldown_by_default() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let pool = EndpointPool::new(vec![EndpointPoolItem::new(
            EndpointPoolEndpoint::new("primary", "model", 0, 2, Duration::from_secs(2)),
            ScriptedClient {
                id: "primary",
                attempts: Mutex::new(VecDeque::new()),
                calls,
            },
        )]);
        let strategy = Arc::new(ConcurrentSuccessAfterFailureStrategy {
            calls: AtomicUsize::new(0),
            first_started: Notify::new(),
            release_first: Notify::new(),
        });

        let first_pool = pool.clone();
        let first_strategy = Arc::clone(&strategy);
        let first = tokio::spawn(async move { first_pool.route(first_strategy.as_ref()).await });
        strategy.first_started.notified().await;

        let second = pool
            .route(strategy.as_ref())
            .await
            .expect_err("second concurrent failure should cool down endpoint");
        strategy.release_first.notify_waiters();
        let first = first
            .await
            .expect("join first route")
            .expect("first success");
        let third = pool
            .route(strategy.as_ref())
            .await
            .expect_err("newer cooldown should survive older success");

        assert_eq!(first, "success");
        assert_eq!(second, ScriptError::CoolingDown(Duration::from_secs(60)));
        assert!(matches!(
            third,
            ScriptError::CoolingDown(retry_after) if retry_after <= Duration::from_secs(60)
        ));
        assert_eq!(strategy.calls.load(Ordering::SeqCst), 2);
    }
}
