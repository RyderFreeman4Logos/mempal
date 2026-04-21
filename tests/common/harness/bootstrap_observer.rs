//! Helpers for asserting daemon bootstrap event ordering via
//! `tokio::sync::mpsc`.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use mempal::bootstrap_events::BootstrapEvent;
use tokio::sync::mpsc;

pub struct BootstrapObserver {
    rx: mpsc::Receiver<BootstrapEvent>,
    seen: Vec<BootstrapEvent>,
}

pub fn channel() -> (mpsc::Sender<BootstrapEvent>, BootstrapObserver) {
    let (tx, rx) = mpsc::channel(16);
    (
        tx,
        BootstrapObserver {
            rx,
            seen: Vec::new(),
        },
    )
}

impl BootstrapObserver {
    pub async fn recv(&mut self) -> Option<BootstrapEvent> {
        let event = self.rx.recv().await?;
        self.seen.push(event);
        Some(event)
    }

    pub async fn recv_until(
        &mut self,
        expected: BootstrapEvent,
        timeout: Duration,
    ) -> Result<Vec<BootstrapEvent>> {
        let deadline = tokio::time::Instant::now() + timeout;
        while tokio::time::Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let event = tokio::time::timeout(remaining, self.recv())
                .await
                .context("timed out waiting for bootstrap event")?;
            match event {
                Some(event) if event == expected => return Ok(self.seen.clone()),
                Some(_) => continue,
                None => bail!("bootstrap event channel closed before {expected:?}"),
            }
        }

        bail!("timed out waiting for bootstrap event {expected:?}")
    }

    pub fn seen(&self) -> &[BootstrapEvent] {
        &self.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn smoke_receives_ready_event() {
        let (tx, mut observer) = channel();
        tx.send(BootstrapEvent::Daemonize)
            .await
            .expect("send daemonize");
        tx.send(BootstrapEvent::Ready).await.expect("send ready");

        let seen = observer
            .recv_until(BootstrapEvent::Ready, Duration::from_secs(1))
            .await
            .expect("observe ready");

        assert_eq!(seen, vec![BootstrapEvent::Daemonize, BootstrapEvent::Ready]);
    }
}
