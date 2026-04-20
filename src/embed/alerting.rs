use std::path::Path;
use std::process::{Command, Stdio};

pub fn fire_alert(script_path: &Path, fail_count: u64, error_message: &str) {
    if !script_path.is_absolute() {
        return;
    }

    if let Err(error) = Command::new(script_path)
        .arg(fail_count.to_string())
        .arg(error_message)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        eprintln!(
            "mempal: failed to spawn embed alert script {}: {error}",
            script_path.display()
        );
    }
}
