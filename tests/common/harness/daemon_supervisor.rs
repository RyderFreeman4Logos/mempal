//! Spawn and supervise `mempal daemon` child processes for integration tests.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

pub struct DaemonSupervisor {
    child: Child,
    pid: i32,
    db_path: PathBuf,
    stdout_lines: Arc<Mutex<Vec<String>>>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stdout_task: Option<tokio::task::JoinHandle<()>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl DaemonSupervisor {
    pub async fn spawn(mut env_vars: HashMap<String, String>, args: Vec<String>) -> Result<Self> {
        if !env_vars.contains_key(mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV)
            && let Some(home) = env_vars.get("HOME")
        {
            let runtime_root = std::path::Path::new(home).join(".mempal").join("runtime");
            env_vars.insert(
                mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV.to_string(),
                runtime_root.display().to_string(),
            );
        }
        let db_path = match env_vars
            .get("HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        {
            Some(home) => home.join(".mempal").join("palace.db"),
            None => bail!("daemon supervisor requires HOME for readiness"),
        };
        let mut command = Command::new(env!("CARGO_BIN_EXE_mempal"));
        command.arg("daemon");
        command.args(args);
        command.envs(env_vars);
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        // harness-point: PR0
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn()?;
        let pid = child.id().expect("daemon pid") as i32;
        let stdout = child.stdout.take().expect("daemon stdout");
        let stderr = child.stderr.take().expect("daemon stderr");
        let stdout_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));

        let stdout_target = Arc::clone(&stdout_lines);
        let stdout_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                stdout_target.lock().await.push(line);
            }
        });

        let stderr_target = Arc::clone(&stderr_lines);
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                stderr_target.lock().await.push(line);
            }
        });

        Ok(Self {
            child,
            pid,
            db_path,
            stdout_lines,
            stderr_lines,
            stdout_task: Some(stdout_task),
            stderr_task: Some(stderr_task),
        })
    }

    pub async fn wait_ready(&self, timeout: Duration) -> Result<()> {
        let db_path = self.db_path.clone();
        let daemon_pid =
            tokio::task::spawn_blocking(move || mempal::daemon_readiness::wait(&db_path, timeout))
                .await??;
        if daemon_pid != self.pid {
            bail!("readiness probe identified a different daemon process");
        }
        Ok(())
    }

    pub fn sigterm(&self) {
        unsafe {
            libc::kill(-self.pid, libc::SIGTERM);
        }
    }

    pub fn sigkill(&self) {
        unsafe {
            libc::kill(-self.pid, libc::SIGKILL);
        }
    }

    pub async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await?;
        if let Some(task) = self.stdout_task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        Ok(status)
    }

    pub async fn stdout_lines(&self) -> Vec<String> {
        self.stdout_lines.lock().await.clone()
    }

    pub async fn stderr_lines(&self) -> Vec<String> {
        self.stderr_lines.lock().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_binary_path_is_available() {
        assert!(env!("CARGO_BIN_EXE_mempal").contains("mempal"));
    }

    #[tokio::test]
    async fn wait_ready_rejects_log_path_without_lifecycle_readiness() -> Result<()> {
        let child = Command::new("sleep").arg("60").spawn()?;
        let pid = child.id().expect("sleep pid") as i32;
        let stdout_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_lines = Arc::new(Mutex::new(vec!["daemon log path: test".to_string()]));
        let temp = tempfile::tempdir()?;
        let db_path = temp.path().join("palace.db");
        let mut supervisor = DaemonSupervisor {
            child,
            pid,
            db_path,
            stdout_lines,
            stderr_lines,
            stdout_task: None,
            stderr_task: None,
        };

        let readiness = supervisor.wait_ready(Duration::from_millis(100)).await;
        supervisor.child.kill().await?;
        let status = supervisor.wait().await?;
        assert!(!status.success());
        readiness.expect_err("a log-path diagnostic is not daemon readiness");
        Ok(())
    }
}
