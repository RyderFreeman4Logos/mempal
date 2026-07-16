use super::{ProcessLiveness, retain_holder_for_liveness};

#[test]
fn unverifiable_foreign_process_is_retained_fail_closed() {
    assert!(retain_holder_for_liveness(ProcessLiveness::Unverifiable));
}

#[test]
fn confirmed_dead_foreign_process_is_reclaimable() {
    assert!(!retain_holder_for_liveness(ProcessLiveness::Dead));
}
