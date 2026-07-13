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
        state.record_recovered(now + 1);
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
