use std::process::Command;

use super::repo_root;

fn run_harness(harness: &str, description: &str) {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run absent-snapshot cleanup harness");
    assert!(
        output.status.success(),
        "{description}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cargo_test_wrapper_proves_cleanup_when_an_owned_snapshot_disappears() {
    run_harness(
        r#"
import importlib.util
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
supervisor = module.Supervisor(child, 1)
absent = module.Identity(424242, 1)
supervisor.owned[424242] = module.OwnedProcess(absent, None, False)
supervisor.seen_identities[424242] = absent
original_scan = module.scan_snapshots
try:
    module.scan_snapshots = lambda: {}
    snapshots = supervisor.discover()
    assert 424242 not in supervisor.owned
    assert supervisor.live_status(snapshots) == (False, False)
    assert not supervisor.ownership_uncertain
finally:
    module.scan_snapshots = original_scan
    supervisor.close()
    child.terminate()
    child.wait()
"#,
        "an absent owned snapshot must prove the process exited",
    );
}

#[test]
fn cargo_test_wrapper_omits_absent_owned_identities_from_cleanup_dump() {
    run_harness(
        r#"
import contextlib
import importlib.util
import io
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
supervisor = module.Supervisor(child, 1)
absent = module.Identity(424242, 1)
supervisor.owned[424242] = module.OwnedProcess(absent, None, False)
original_scan = module.scan_snapshots
try:
    module.scan_snapshots = lambda: {}
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr):
        supervisor.process_cleanup_failure()
    output = stderr.getvalue()
    assert "remaining owned processes:" in output
    assert "pid=424242" not in output
    assert "comm='?'" not in output
finally:
    module.scan_snapshots = original_scan
    supervisor.close()
    child.terminate()
    child.wait()
"#,
        "cleanup diagnostics must omit absent owned identities",
    );
}
