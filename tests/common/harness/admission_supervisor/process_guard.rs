use std::time::{Duration, Instant};

use super::{DeadlineChild, ProcessIdentity, SpawnSpec};

#[derive(Debug)]
pub(crate) struct ExactProcessGuard {
    identity: ProcessIdentity,
}

impl ExactProcessGuard {
    pub(super) fn new(identity: ProcessIdentity) -> Self {
        Self { identity }
    }

    pub(super) fn assert_gone(self, context: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.identity.still_refers_to_original_process() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            !self.identity.still_refers_to_original_process(),
            "{context}: original process {:?} is still present",
            self.identity
        );
    }

    fn fallback_target_after_observation(
        identity: ProcessIdentity,
        after_observation: impl FnOnce(),
    ) -> Option<libc::pid_t> {
        if identity.still_refers_to_original_process() {
            after_observation();
        }
        // This guard never owns a signal authority. The callback is a deterministic regression
        // seam: it revokes an observed identity before this method would otherwise yield a PID.
        None
    }
}

pub(crate) fn process_identity(pid: libc::pid_t) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_time_ticks: Some(read_start_time(pid)),
    }
}

fn read_start_time(pid: libc::pid_t) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("read process stat");
    let (_, fields) = stat.rsplit_once(") ").expect("process stat fields");
    fields
        .split_whitespace()
        .nth(19)
        .expect("process start-time field")
        .parse()
        .expect("numeric process start time")
}

#[test]
fn identity_revocation_after_observation_never_yields_a_bare_signal_target() {
    let mut spec = SpawnSpec::new("/bin/sleep").expect("absolute sleep executable");
    spec.arg("30");
    let mut fixture =
        DeadlineChild::spawn(spec, Duration::from_secs(2)).expect("spawn supervised fixture");
    let identity = fixture.identity();
    assert!(
        identity.still_refers_to_original_process(),
        "fixture must be observable before revocation"
    );

    let target = ExactProcessGuard::fallback_target_after_observation(identity, || {
        let cleanup = fixture
            .force_kill_with_timeout(Duration::from_secs(2))
            .expect_complete("fence and reap owned fixture");
        assert!(cleanup.kill_fence_sent);
        assert!(cleanup.errors.is_empty(), "{cleanup:#?}");
        assert!(
            !identity.still_refers_to_original_process(),
            "fixture identity must be revoked before a signal target can be used"
        );
    });

    assert_eq!(
        target, None,
        "a guard without stable authority must not yield a bare PID for signaling"
    );
}
