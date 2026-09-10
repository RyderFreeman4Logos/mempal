use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use super::common::harness::DaemonSupervisor;

pub(super) async fn wait_for_stderr_line(
    daemon: &DaemonSupervisor,
    timeout: Duration,
    needle: &str,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if daemon
            .stderr_lines()
            .await
            .iter()
            .any(|line| line.contains(needle))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon stderr did not contain {needle:?} within {timeout:?}");
}

pub(super) async fn spawn_status(home: &Path) -> DaemonSupervisor {
    let daemon = DaemonSupervisor::spawn(
        HashMap::from([("HOME".to_string(), home.display().to_string())]),
        vec!["--foreground".to_string()],
    )
    .await
    .expect("spawn status daemon");
    wait_for_stderr_line(
        &daemon,
        Duration::from_secs(5),
        "hooks not enabled; daemon running configured background services only",
    )
    .await;
    daemon
}

pub(super) async fn stop_status(daemon: &mut DaemonSupervisor) {
    daemon.sigterm();
    let status = tokio::time::timeout(Duration::from_secs(3), daemon.wait())
        .await
        .expect("status daemon did not stop within deadline")
        .expect("wait status daemon");
    assert!(status.success(), "status daemon exited with {status:?}");
}
