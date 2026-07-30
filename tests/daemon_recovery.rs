use std::fs;

use mempal::daemon_bootstrap::DaemonContext;
use mempal::daemon_recovery::{
    DaemonRecovery, DaemonRecoveryFaultReporter, DaemonRecoveryState, RecoveryFault, RecoveryPhase,
    RestartDecision,
};

const WINDOW_SECS: u64 = 600;
const COOLDOWN_SECS: u64 = 900;
const MAX_RESTARTS: usize = 3;

#[test]
fn repeated_faults_exhaust_restart_budget_until_cooldown_ends() {
    let mut state = DaemonRecoveryState::default();

    for now in [100, 200] {
        assert_eq!(
            state.record_fault(RecoveryFault::WriterLeaseLost, now),
            RestartDecision::RestartAllowed
        );
    }
    assert_eq!(state.snapshot(250).restart_budget_remaining, 1);

    assert_eq!(
        state.record_fault(RecoveryFault::DatabaseLocked, 300),
        RestartDecision::CooldownRequired
    );
    let exhausted = state.snapshot(300);
    assert_eq!(exhausted.phase, RecoveryPhase::Cooldown);
    assert_eq!(exhausted.restart_budget_remaining, 0);
    assert_eq!(exhausted.cooldown_remaining_secs, COOLDOWN_SECS);
    assert_eq!(
        state
            .snapshot(300 + WINDOW_SECS + 1)
            .restart_budget_remaining,
        0,
        "cooldown remains authoritative after rolling-window entries age out"
    );

    assert_eq!(
        state.admit_start(300 + COOLDOWN_SECS - 1),
        RestartDecision::CooldownRequired
    );
    assert_eq!(
        state.admit_start(300 + COOLDOWN_SECS),
        RestartDecision::RestartAllowed
    );
}

#[test]
fn healthy_generation_replenishes_restart_budget() {
    let mut state = DaemonRecoveryState::default();
    for now in [100, 200] {
        assert_eq!(state.admit_start(now), RestartDecision::RestartAllowed);
        // Pre-admission faults from a prior failed attempt must be dropped once
        // this generation reaches a successful recovery mark.
        assert_eq!(
            state.record_fault(RecoveryFault::WriterLeaseLost, now.saturating_sub(1).max(1)),
            RestartDecision::RestartAllowed
        );
        // Re-admit so the fault above is treated as prior-generation.
        assert_eq!(state.admit_start(now), RestartDecision::RestartAllowed);
        state.record_recovered(now + 1);
    }

    let recovered = state.snapshot(250);
    assert_eq!(recovered.phase, RecoveryPhase::Healthy);
    assert_eq!(recovered.recent_fault_count, 0);
    assert_eq!(recovered.restart_budget_remaining, MAX_RESTARTS);
    assert_eq!(
        state.record_fault(RecoveryFault::DatabaseLocked, 300),
        RestartDecision::RestartAllowed,
        "the next transient fault starts a fresh recovery window"
    );
}

#[test]
fn record_recovered_does_not_clear_faults_charged_to_the_current_generation() {
    let mut state = DaemonRecoveryState::default();
    assert_eq!(state.admit_start(100), RestartDecision::RestartAllowed);
    assert_eq!(
        state.record_fault(RecoveryFault::WriteStall, 150),
        RestartDecision::RestartAllowed
    );
    // Startup can still call record_recovered after a same-generation fault
    // (for example on the hooks-disabled early-return path after a lease
    // fault already requested shutdown). The fault must remain charged so
    // the rolling restart budget accumulates across supervisor restarts.
    state.record_recovered(160);
    let after = state.snapshot(160);
    assert_eq!(after.phase, RecoveryPhase::Recovering);
    assert_eq!(after.recent_fault_count, 1);
    assert_eq!(after.restart_budget_remaining, MAX_RESTARTS - 1);
    assert_eq!(after.last_fault, Some(RecoveryFault::WriteStall));

    // Next generation admits: prior-generation faults remain until this
    // generation recovers *without* charging a post-admission fault.
    assert_eq!(state.admit_start(200), RestartDecision::RestartAllowed);
    assert_eq!(
        state.snapshot(200).recent_fault_count,
        1,
        "admitting a replacement generation must not wipe prior charged faults"
    );
    // Successful recovery on the replacement generation clears only
    // pre-admission faults (the prior generation's charges).
    state.record_recovered(210);
    let replenished = state.snapshot(210);
    assert_eq!(replenished.phase, RecoveryPhase::Healthy);
    assert_eq!(replenished.recent_fault_count, 0);
    assert_eq!(replenished.restart_budget_remaining, MAX_RESTARTS);
}

#[test]
fn healthy_generation_replenishes_budget_when_prior_fault_shares_admit_unix_second() {
    let mut state = DaemonRecoveryState::default();
    // Prior generation charges a fault, then a replacement admits in the same
    // wall-clock second. Epoch tracking must still treat the fault as prior.
    assert_eq!(
        state.record_fault(RecoveryFault::WriteStall, 100),
        RestartDecision::RestartAllowed
    );
    assert_eq!(state.admit_start(100), RestartDecision::RestartAllowed);
    state.record_recovered(101);
    let recovered = state.snapshot(101);
    assert_eq!(recovered.phase, RecoveryPhase::Healthy);
    assert_eq!(recovered.recent_fault_count, 0);
    assert_eq!(recovered.restart_budget_remaining, MAX_RESTARTS);
    assert_eq!(recovered.last_fault, None);
}

#[test]
fn faults_reported_during_cooldown_do_not_extend_the_cooldown() {
    let mut state = DaemonRecoveryState::default();
    for now in [100, 200, 300] {
        state.record_fault(RecoveryFault::DatabaseLocked, now);
    }
    let initial = state.snapshot(300);
    assert_eq!(initial.phase, RecoveryPhase::Cooldown);
    assert_eq!(initial.cooldown_remaining_secs, COOLDOWN_SECS);
    assert_eq!(initial.last_fault, Some(RecoveryFault::DatabaseLocked));

    assert_eq!(
        state.record_fault(RecoveryFault::WriteStall, 301),
        RestartDecision::CooldownRequired
    );
    let during_cooldown = state.snapshot(301);
    assert_eq!(during_cooldown.recent_fault_count, MAX_RESTARTS);
    assert_eq!(during_cooldown.cooldown_remaining_secs, COOLDOWN_SECS - 1);
    assert_eq!(
        during_cooldown.last_fault,
        Some(RecoveryFault::DatabaseLocked),
        "a cooldown-blocked generation must not overwrite the root-cause diagnostic"
    );

    assert_eq!(
        state.admit_start(300 + COOLDOWN_SECS),
        RestartDecision::RestartAllowed,
        "the original cooldown deadline admits the next daemon generation"
    );
}

#[test]
fn late_cooldown_fault_reports_do_not_prune_or_rewrite_frozen_fault_state() {
    let mut state = DaemonRecoveryState::default();
    for now in [100, 200, 300] {
        state.record_fault(RecoveryFault::DatabaseLocked, now);
    }
    let entered = state.snapshot(300);
    assert_eq!(entered.phase, RecoveryPhase::Cooldown);
    assert_eq!(entered.recent_fault_count, MAX_RESTARTS);
    assert_eq!(entered.cooldown_remaining_secs, COOLDOWN_SECS);
    assert_eq!(entered.last_fault, Some(RecoveryFault::DatabaseLocked));

    // After the rolling window has elapsed (600s) but before the fixed 900s
    // cooldown deadline, a fault report must not prune or rewrite state.
    let late = 300 + WINDOW_SECS + 1; // 901; cooldown ends at 1200
    assert!(late < 300 + COOLDOWN_SECS);
    assert_eq!(
        state.record_fault(RecoveryFault::WriteStall, late),
        RestartDecision::CooldownRequired
    );
    let frozen = state.snapshot(late);
    assert_eq!(frozen.phase, RecoveryPhase::Cooldown);
    assert_eq!(
        frozen.recent_fault_count, MAX_RESTARTS,
        "active cooldown must freeze fault history even after the rolling window elapses"
    );
    assert_eq!(
        frozen.cooldown_remaining_secs,
        (300 + COOLDOWN_SECS).saturating_sub(late)
    );
    assert_eq!(
        frozen.last_fault,
        Some(RecoveryFault::DatabaseLocked),
        "a late cooldown-blocked report must not overwrite the root-cause diagnostic"
    );
    assert_eq!(
        state.admit_start(300 + COOLDOWN_SECS - 1),
        RestartDecision::CooldownRequired
    );
    assert_eq!(
        state.admit_start(300 + COOLDOWN_SECS),
        RestartDecision::RestartAllowed
    );
}

#[test]
fn old_faults_leave_the_rolling_window_without_resetting_active_recovery() {
    let mut state = DaemonRecoveryState::default();
    assert_eq!(
        state.record_fault(RecoveryFault::WriteStall, 10),
        RestartDecision::RestartAllowed
    );

    let snapshot = state.snapshot(10 + WINDOW_SECS + 1);
    assert_eq!(snapshot.phase, RecoveryPhase::Recovering);
    assert_eq!(snapshot.restart_budget_remaining, MAX_RESTARTS);

    state.record_recovered(10 + WINDOW_SECS + 2);
    assert_eq!(
        state.snapshot(10 + WINDOW_SECS + 2).phase,
        RecoveryPhase::Healthy
    );
}

#[test]
fn restart_budget_is_shared_by_new_controller_instances() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let first = DaemonRecovery::new(tempdir.path());
    assert_eq!(
        first
            .record_fault(RecoveryFault::WriterLeaseLost)
            .expect("record fault"),
        RestartDecision::RestartAllowed
    );

    let replacement = DaemonRecovery::new(tempdir.path());
    let snapshot = replacement.snapshot().expect("reload recovery state");
    assert_eq!(snapshot.recent_fault_count, 1);
    assert_eq!(snapshot.phase, RecoveryPhase::Recovering);
    assert_eq!(snapshot.restart_budget_remaining, MAX_RESTARTS - 1);
}

#[test]
fn one_process_generation_consumes_at_most_one_fault_slot() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let recovery = DaemonRecovery::new(tempdir.path());
    let reporter = DaemonRecoveryFaultReporter::new(recovery.clone());

    reporter.record_fault_once(RecoveryFault::WriteStall);
    reporter.record_fault_once(RecoveryFault::WriterLeaseLost);

    let snapshot = recovery.snapshot().expect("recovery snapshot");
    assert_eq!(snapshot.recent_fault_count, 1);
    assert_eq!(snapshot.last_fault, Some(RecoveryFault::WriteStall));
}

#[test]
fn restart_budget_admission_precedes_storage_bootstrap() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let mempal_home = tempdir.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let config_path = mempal_home.join("config.toml");
    fs::write(
        &config_path,
        format!(
            r#"
db_path = "{}"

[embedder]
backend = "stub"

[daemon]
log_path = "{}"
"#,
            db_path.display(),
            mempal_home.join("daemon.log").display()
        ),
    )
    .expect("write config");

    let recovery = DaemonRecovery::new(&mempal_home);
    let mut final_decision = RestartDecision::RestartAllowed;
    for _ in 0..mempal::daemon_recovery::MAX_RESTARTS_PER_WINDOW {
        final_decision = recovery
            .record_fault(RecoveryFault::DatabaseLocked)
            .expect("record recovery fault");
    }
    assert_eq!(final_decision, RestartDecision::CooldownRequired);

    let runtime_root = tempdir.path().join("runtime");
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let error = match DaemonContext::bootstrap_with_events_for_test(
        config_path,
        true,
        Some(tx),
        &runtime_root,
    ) {
        Ok(context) => {
            drop(context);
            panic!("cooldown must reject daemon bootstrap")
        }
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("daemon restart budget exhausted"),
        "unexpected bootstrap error: {error:#}"
    );
    assert!(
        rx.blocking_recv().is_none(),
        "cooldown must reject bootstrap before daemonizing or opening SQLite"
    );
    assert!(!db_path.exists(), "cooldown must prevent database creation");
}
