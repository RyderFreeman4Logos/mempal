use super::local_gate_child::{RecordedProcessIdentity, capture_recorded_process};
use super::*;

pub(super) fn recorded_process_identity(record: &str) -> RecordedProcessIdentity {
    let mut fields = record.split_ascii_whitespace();
    let pid = fields
        .next()
        .expect("recorded process PID")
        .parse()
        .expect("numeric recorded process PID");
    let start_time_ticks = fields
        .next()
        .expect("recorded process start time")
        .parse()
        .expect("numeric recorded process start time");
    assert!(
        fields.next().is_none(),
        "recorded process identity contains unexpected fields"
    );
    RecordedProcessIdentity {
        pid,
        start_time_ticks,
    }
}

fn process_start_time_ticks(pid: i32) -> Option<u64> {
    let stat = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let fields = stat.rsplit(|byte| *byte == b')').next()?;
    std::str::from_utf8(fields)
        .ok()?
        .split_ascii_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

pub(super) fn sleeper_processes(timeout_secs: &str) -> Vec<RecordedProcessIdentity> {
    let ps = Command::new("ps")
        .args(["-eo", "pid=,args="])
        .output()
        .expect("run ps");
    let stdout = String::from_utf8_lossy(&ps.stdout);
    let expected = format!("sleep {timeout_secs}");
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (pid, args) = trimmed.split_once(' ')?;
            (args.trim() == expected).then(|| {
                let pid = pid.parse().expect("numeric sleep PID");
                RecordedProcessIdentity {
                    pid,
                    start_time_ticks: process_start_time_ticks(pid)
                        .expect("sleep process start time remains available"),
                }
            })
        })
        .collect()
}

pub(super) fn terminate_recorded_processes(processes: &[RecordedProcessIdentity]) {
    for expected in processes {
        if let Some(process) = capture_recorded_process(*expected)
            .expect("re-verify recorded process identity before signaling")
        {
            process
                .send_signal(libc::SIGTERM)
                .expect("signal recorded process through pidfd");
        }
    }
}

#[test]
fn recorded_pid_reuse_is_refused_without_signaling_the_live_process() {
    let fixture = tempfile::tempdir().expect("create PID-reuse fixture");
    let identity_file = fixture.path().join("identity");
    let ready_file = fixture.path().join("ready");
    let term_file = fixture.path().join("term");
    let mut command = Command::new("/bin/bash");
    command
        .args([
            "-c",
            r#"
                trap ': >"${TERM_FILE:?}"; exit 0' TERM
                pid="${BASHPID}"
                start_time="$(awk '{print $22}' "/proc/${pid}/stat")"
                printf '%s %s\n' "${pid}" "${start_time}" >"${IDENTITY_FILE:?}"
                : >"${READY_FILE:?}"
                while true; do /bin/sleep 60; done
            "#,
        ])
        .env("IDENTITY_FILE", &identity_file)
        .env("READY_FILE", &ready_file)
        .env("TERM_FILE", &term_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = spawn_in_own_session(&mut command).expect("spawn PID-reuse fixture");
    wait_for_file(&ready_file, Duration::from_secs(2), "PID-reuse fixture");
    let original = recorded_process_identity(
        &fs::read_to_string(&identity_file).expect("read live fixture identity"),
    );
    let recycled = RecordedProcessIdentity {
        pid: original.pid,
        start_time_ticks: original
            .start_time_ticks
            .checked_add(1)
            .expect("fixture start time has headroom"),
    };

    terminate_recorded_processes(&[recycled]);
    thread::sleep(Duration::from_millis(50));

    assert!(
        !term_file.exists(),
        "a mismatched starttime must refuse the signal intended for a recycled PID"
    );
    assert!(
        capture_recorded_process(original)
            .expect("inspect original fixture identity")
            .expect("original fixture identity must remain live")
            .is_running()
            .expect("check original fixture liveness"),
        "a PID-reuse attempt must not signal the live process"
    );
    drop(child);
}
