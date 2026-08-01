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
        while identity_is_running(self.identity) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(
            !identity_is_running(self.identity),
            "{context}: original process {:?} is still running",
            self.identity
        );
    }

    fn fallback_target_after_observation(
        identity: ProcessIdentity,
        after_observation: impl FnOnce(),
    ) -> Option<libc::pid_t> {
        if identity_is_running(identity) {
            after_observation();
        }
        // This guard never owns a signal authority. The callback is a deterministic regression
        // seam: it revokes an observed identity before this method would otherwise yield a PID.
        None
    }
}

fn identity_is_running(identity: ProcessIdentity) -> bool {
    if !mempal::process_is_live(identity.pid) {
        return false;
    }
    let Some(expected) = identity.start_time_ticks else {
        return true;
    };
    match read_start_time(identity.pid) {
        Ok(actual) => actual == expected,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
}

pub(crate) fn process_identity(pid: libc::pid_t) -> ProcessIdentity {
    ProcessIdentity {
        pid,
        start_time_ticks: Some(read_start_time(pid).expect("read process stat")),
    }
}

fn read_start_time(pid: libc::pid_t) -> std::io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat.rsplit_once(") ").expect("process stat fields");
    Ok(fields
        .split_whitespace()
        .nth(19)
        .expect("process start-time field")
        .parse()
        .expect("numeric process start time"))
}

#[test]
fn zombie_identity_is_not_live_for_assert_gone() {
    let mut child = std::process::Command::new("/bin/true")
        .spawn()
        .expect("spawn short-lived fixture");
    let identity = process_identity(child.id() as libc::pid_t);
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::fs::read_to_string(format!("/proc/{}/stat", identity.pid))
        .ok()
        .is_some_and(|stat| stat.split_whitespace().nth(2) != Some("Z"))
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    let stat =
        std::fs::read_to_string(format!("/proc/{}/stat", identity.pid)).expect("zombie stat");
    let state = stat.split_whitespace().nth(2);
    let is_zombie = state == Some("Z");
    let result = is_zombie.then(|| {
        std::panic::catch_unwind(|| {
            ExactProcessGuard::new(identity).assert_gone("zombie fixture");
        })
    });
    child.wait().expect("reap zombie fixture");
    assert!(
        is_zombie,
        "fixture must remain unreaped for this liveness check"
    );
    assert!(
        result
            .expect("run the guard only after confirming the zombie state")
            .is_ok(),
        "zombie identity must not be treated as live"
    );
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
