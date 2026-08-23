#[cfg(test)]
mod monitor_tests {
    use super::*;

    #[test]
    fn descendant_monitor_delayed_start_is_joined_after_release() {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("60")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = spawn_in_own_session(&mut command).expect("spawn monitor root");
        let root = capture_owned_child(child.child()).expect("capture monitor root identity");
        let (start_release_sender, start_release_receiver) = mpsc::channel();
        let monitor_result = DescendantMonitor::spawn_delayed_for_test(
            root.identity,
            start_release_receiver,
        );
        start_release_sender
            .send(())
            .expect("release delayed monitor worker");
        let mut monitor =
            monitor_result.expect("a delayed monitor worker must not fail construction");
        let mut tracked_processes = Vec::new();

        monitor
            .stop_and_drain(
                &mut tracked_processes,
                Instant::now() + Duration::from_secs(1),
            )
            .expect("release and join delayed monitor worker");
        reap_owned_child(child).expect("reap monitor root");
    }
}
