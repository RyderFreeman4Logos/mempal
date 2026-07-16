//! Daemon-owned periodic sleep/consolidation scheduling.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::core::{config::Config, db::Database, types::RuntimeWriterLease};
use crate::sleep::{SleepCycleSummary, SleepPhaseSelection, SleepRunOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Schedule {
    interval: Duration,
    phases: SleepPhaseSelection,
}

impl Schedule {
    fn from_config(config: &Config) -> Result<Option<Self>> {
        if !config.sleep.enabled || config.sleep.auto_interval_secs == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            interval: Duration::from_secs(config.sleep.auto_interval_secs),
            phases: parse_phases(&config.sleep.phases)?,
        }))
    }
}

pub(super) fn spawn(
    db_path: PathBuf,
    config: Arc<Config>,
    writer_lease: RuntimeWriterLease,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let Some(schedule) = Schedule::from_config(config.as_ref())? else {
        tracing::info!(
            sleep_enabled = config.sleep.enabled,
            auto_interval_secs = config.sleep.auto_interval_secs,
            "daemon embedded sleep scheduler disabled"
        );
        return Ok(None);
    };
    let phase_names = phase_names(schedule.phases);
    tracing::info!(
        auto_interval_secs = schedule.interval.as_secs(),
        phases = %phase_names,
        "daemon embedded sleep scheduler started"
    );

    Ok(Some(tokio::spawn(async move {
        loop {
            super::wait_for_shutdown_or_sleep(schedule.interval).await;
            if super::shutdown_requested() {
                break;
            }
            let cycle_db_path = db_path.clone();
            let cycle_config = Arc::clone(&config);
            let cycle_lease = writer_lease.clone();
            tracing::info!("daemon embedded sleep cycle started");
            let result = tokio::task::spawn_blocking(move || {
                wait_for_test_cycle_release();
                run_cycle(cycle_db_path, cycle_config, cycle_lease, schedule.phases)
            })
            .await;
            match result {
                Ok(Ok(summary)) => log_summary(&summary),
                Ok(Err(error)) => {
                    tracing::warn!(?error, "daemon embedded sleep cycle failed");
                }
                Err(error) => {
                    tracing::warn!(?error, "daemon embedded sleep cycle task failed");
                }
            }
        }
    })))
}

pub(super) async fn drain(handle: Option<tokio::task::JoinHandle<()>>) {
    if let Some(handle) = handle {
        handle.abort();
        let _ = handle.await;
    }
}

#[cfg(debug_assertions)]
fn wait_for_test_cycle_release() {
    let Some(block_path) = std::env::var_os("MEMPAL_TEST_SLEEP_CYCLE_BLOCK_FILE") else {
        return;
    };
    while std::path::Path::new(&block_path).exists() {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(debug_assertions))]
fn wait_for_test_cycle_release() {}

fn run_cycle(
    db_path: PathBuf,
    config: Arc<Config>,
    writer_lease: RuntimeWriterLease,
    phases: SleepPhaseSelection,
) -> Result<SleepCycleSummary> {
    let db = Database::open(&db_path)
        .with_context(|| format!("failed to open sleep database {}", db_path.display()))?;
    crate::sleep::run_sleep_cycle_with_writer_lease(
        &db,
        config.as_ref(),
        SleepRunOptions {
            phases,
            dry_run: false,
            project_id: None,
        },
        Some(&writer_lease),
    )
    .context("scheduled sleep cycle failed")
}

fn parse_phases(configured: &[String]) -> Result<SleepPhaseSelection> {
    if configured.is_empty() {
        bail!("sleep.phases must contain at least one of: nrem, rem, salience");
    }
    let mut selection = SleepPhaseSelection::default();
    for phase in configured {
        let selected = match phase.as_str() {
            "nrem" => &mut selection.nrem,
            "rem" => &mut selection.rem,
            "salience" => &mut selection.salience,
            other => {
                bail!("unsupported sleep phase `{other}`; expected one of: nrem, rem, salience")
            }
        };
        if std::mem::replace(selected, true) {
            bail!("duplicate sleep phase `{phase}`");
        }
    }
    Ok(selection)
}

fn phase_names(phases: SleepPhaseSelection) -> String {
    phases
        .selected_or_all()
        .into_iter()
        .map(|phase| phase.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn log_summary(summary: &SleepCycleSummary) {
    tracing::info!(
        phases = %summary
            .phases
            .iter()
            .map(|phase| phase.as_str())
            .collect::<Vec<_>>()
            .join(","),
        processed = summary.processed_count(),
        pruned = summary.pruned_count(),
        compacted = summary.compacted_count(),
        conflicts_resolved = summary.conflicts_resolved_count(),
        salience_scored = summary.salience_scored_count(),
        crystallize_candidates = summary.crystallize_candidates,
        crystallized_cards = summary.crystallized_cards,
        "daemon embedded sleep cycle completed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_is_disabled_by_default() {
        assert!(Schedule::from_config(&Config::default()).unwrap().is_none());
    }

    #[test]
    fn schedule_uses_configured_interval_and_phases() {
        let mut config = Config::default();
        config.sleep.auto_interval_secs = 17;
        config.sleep.phases = vec!["rem".into(), "salience".into()];

        let schedule = Schedule::from_config(&config).unwrap().unwrap();

        assert_eq!(schedule.interval, Duration::from_secs(17));
        assert_eq!(
            schedule.phases,
            SleepPhaseSelection {
                nrem: false,
                rem: true,
                salience: true,
            }
        );
    }

    #[test]
    fn schedule_rejects_empty_unknown_and_duplicate_phases() {
        for (phases, expected) in [
            (Vec::<String>::new(), "must contain at least one"),
            (vec!["dream".into()], "unsupported sleep phase"),
            (vec!["nrem".into(), "nrem".into()], "duplicate sleep phase"),
        ] {
            let mut config = Config::default();
            config.sleep.auto_interval_secs = 1;
            config.sleep.phases = phases;
            let error = Schedule::from_config(&config).unwrap_err();
            assert!(
                error.to_string().contains(expected),
                "unexpected error: {error}"
            );
        }
    }

    #[tokio::test]
    async fn drain_does_not_wait_for_a_long_running_scheduler_task() {
        let handle = tokio::spawn(std::future::pending());

        tokio::time::timeout(Duration::from_millis(100), drain(Some(handle)))
            .await
            .expect("daemon shutdown must cancel the scheduler task");
    }
}
