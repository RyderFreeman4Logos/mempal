//! JSON-RPC 2.0 client for `mempal serve --mcp` over stdio.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rmcp::model::ServerInfo;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::Instant;

const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(3);
const CLEANUP_RESERVE: Duration = Duration::from_secs(1);
const TERM_GRACE: Duration = Duration::from_millis(250);
const DIAGNOSTIC_TAIL_LINES: usize = 20;

pub struct McpStdio {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    next_id: u64,
    roots: Vec<String>,
    pid: u32,
    reaped: bool,
}

impl McpStdio {
    pub async fn start(db_path: &Path, extra_env: HashMap<String, String>) -> Result<Self> {
        let mempal_home = db_path
            .parent()
            .context("db_path must have a parent mempal home")?;
        let home = mempal_home.parent().unwrap_or(mempal_home);
        let config_path = mempal_home.join("config.toml");
        let embed_base_url = extra_env.get("MEMPAL_TEST_EMBED_BASE_URL").cloned();
        let llm_base_url = extra_env.get("MEMPAL_TEST_LLM_BASE_URL").cloned();
        let embed_section = if let Some(embed_base_url) = embed_base_url {
            format!(
                r#"
[embed]
backend = "openai_compat"
base_url = "{}"
api_model = "test-embed"
dim = 4

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 4
request_timeout_secs = 2
"#,
                embed_base_url, embed_base_url
            )
        } else {
            r#"
[embed]
backend = "stub"
"#
            .to_string()
        };
        let llm_enabled_for = extra_env
            .get("MEMPAL_TEST_LLM_ENABLED_FOR")
            .map(|value| format!("enabled_for = {value}\n"))
            .unwrap_or_default();
        let llm_section = llm_base_url
            .map(|llm_base_url| {
                format!(
                    r#"
[llm]
enabled = true
base_url = "{}"
model = "test-llm"
{}
"#,
                    llm_base_url, llm_enabled_for
                )
            })
            .unwrap_or_default();
        let config = format!(
            r#"
db_path = "{}"
{}{}
[hooks]
enabled = true

[daemon]
log_path = "{}"
"#,
            db_path.display(),
            embed_section,
            llm_section,
            mempal_home.join("daemon.log").display()
        );
        tokio::fs::create_dir_all(mempal_home)
            .await
            .with_context(|| format!("create {}", mempal_home.display()))?;
        tokio::fs::write(&config_path, config)
            .await
            .with_context(|| format!("write {}", config_path.display()))?;

        let mut command = Command::new(env!("CARGO_BIN_EXE_mempal"));
        command.args(["serve", "--mcp"]);
        command.env("HOME", home);
        command.envs(extra_env);

        Self::spawn_command(&mut command)
    }

    pub fn spawn_command(command: &mut Command) -> Result<Self> {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);
        // SAFETY: the post-fork closure calls only the async-signal-safe `setsid` before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command.spawn().context("spawn MCP child")?;
        let pid = child.id().context("missing MCP child PID")?;
        let stdin = child.stdin.take().context("missing child stdin")?;
        let stdout = child.stdout.take().context("missing child stdout")?;
        let stderr = child.stderr.take().context("missing child stderr")?;
        let stderr_lines = Arc::new(Mutex::new(Vec::new()));
        let stderr_target = Arc::clone(&stderr_lines);
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                stderr_target.lock().await.push(line);
            }
        });

        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr_lines,
            stderr_task: Some(stderr_task),
            next_id: 1,
            roots: Vec::new(),
            pid,
            reaped: false,
        })
    }

    pub fn id(&self) -> u32 {
        self.pid
    }

    pub fn is_reaped(&self) -> bool {
        self.reaped
    }

    pub async fn initialize(&mut self) -> Result<ServerInfo> {
        self.initialize_with_roots(&[]).await
    }

    pub async fn initialize_with_roots(&mut self, roots: &[&str]) -> Result<ServerInfo> {
        let deadline = Instant::now() + LIFECYCLE_TIMEOUT;
        let request_deadline = deadline - CLEANUP_RESERVE;
        let initialized = tokio::time::timeout_at(request_deadline, async {
            self.roots = roots.iter().map(|root| root.to_string()).collect();
            let result = self
                .call(
                    "initialize",
                    json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": if roots.is_empty() {
                            json!({})
                        } else {
                            json!({"roots": {"listChanged": true}})
                        },
                        "clientInfo": {
                            "name": "pr0-harness",
                            "version": "0.0.0"
                        }
                    }),
                )
                .await?;
            self.send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/initialized"
            }))
            .await?;
            serde_json::from_value(result).context("decode MCP initialize result")
        })
        .await;

        match initialized {
            Ok(result) => result,
            Err(_) => {
                let cleanup = self.fence_process_group_and_reap(deadline, None).await;
                let diagnostics = self.diagnostics();
                cleanup.with_context(|| {
                    format!("MCP initialize timed out and cleanup failed\n{diagnostics}")
                })?;
                bail!("MCP initialize timed out; child terminated and reaped\n{diagnostics}")
            }
        }
    }

    pub fn set_roots(&mut self, roots: &[&str]) {
        self.roots = roots.iter().map(|root| root.to_string()).collect();
    }

    pub async fn notify_roots_list_changed(&mut self) -> Result<()> {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": "notifications/roots/list_changed"
        }))
        .await
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let message = self.call_raw(method, params).await?;
        if let Some(error) = message.get("error") {
            bail!("JSON-RPC error: {error}");
        }
        Ok(message["result"].clone())
    }

    pub async fn call_raw(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        self.read_response_message(id).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let deadline = Instant::now() + LIFECYCLE_TIMEOUT;
        let request_deadline = deadline - CLEANUP_RESERVE;
        let response_timed_out = tokio::time::timeout_at(request_deadline, async {
            let _ = self.call("shutdown", json!({})).await;
            let _ = self
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/exit"
                }))
                .await;
        })
        .await
        .is_err();

        let graceful_exit_deadline = (!response_timed_out).then_some(request_deadline);
        let cleanup = self
            .fence_process_group_and_reap(deadline, graceful_exit_deadline)
            .await;
        let diagnostics = self.diagnostics();
        if response_timed_out {
            cleanup.with_context(|| {
                format!("MCP shutdown response timed out and cleanup failed\n{diagnostics}")
            })?;
            bail!("MCP shutdown response timed out; child terminated and reaped\n{diagnostics}");
        }
        cleanup.with_context(|| format!("MCP shutdown cleanup failed\n{diagnostics}"))
    }

    pub async fn stderr_lines(&self) -> Vec<String> {
        self.stderr_lines.lock().await.clone()
    }

    async fn fence_process_group_and_reap(
        &mut self,
        deadline: Instant,
        graceful_exit_deadline: Option<Instant>,
    ) -> Result<()> {
        if self.reaped {
            self.finish_stderr(deadline).await;
            return Ok(());
        }

        let (mut leader_exited, mut observe_error) = match graceful_exit_deadline {
            Some(graceful_exit_deadline) => {
                match self
                    .wait_for_leader_exit_unreaped(graceful_exit_deadline)
                    .await
                {
                    Ok(exited) => (exited, None),
                    Err(error) => (false, Some(error)),
                }
            }
            None => (false, None),
        };
        let term_error = if leader_exited {
            None
        } else {
            let error = self.signal_process_group(libc::SIGTERM).err();
            let term_deadline = (Instant::now() + TERM_GRACE).min(deadline);
            match self.wait_for_leader_exit_unreaped(term_deadline).await {
                Ok(exited) => leader_exited = exited,
                Err(error) => observe_error = Some(error),
            }
            error
        };

        // The unreaped leader still owns its numeric PID, so its dedicated PGID cannot be reused
        // between this final fence and the one-and-only reap below.
        let kill_error = self.signal_process_group(libc::SIGKILL).err();
        let reap_error = match tokio::time::timeout_at(deadline, self.child.wait()).await {
            Ok(Ok(_)) => {
                self.reaped = true;
                kill_error.map(|error| {
                    format!(
                        "kill MCP process group {}: {error}; leader_exited={leader_exited}; term={term_error:?}; observe={observe_error:?}",
                        self.pid
                    )
                })
            }
            Ok(Err(error)) => Some(format!(
                "reap MCP child {} after group kill: {error}; leader_exited={leader_exited}; term={term_error:?}; kill={kill_error:?}; observe={observe_error:?}",
                self.pid
            )),
            Err(_) => Some(format!(
                "reap MCP child {} after group kill timed out; leader_exited={leader_exited}; term={term_error:?}; kill={kill_error:?}; observe={observe_error:?}",
                self.pid
            )),
        };
        self.finish_stderr(deadline).await;
        if let Some(error) = reap_error {
            bail!(error);
        }
        Ok(())
    }

    async fn wait_for_leader_exit_unreaped(&self, deadline: Instant) -> io::Result<bool> {
        loop {
            if self.leader_exited_unreaped()? {
                return Ok(true);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(10).min(remaining)).await;
        }
    }

    fn leader_exited_unreaped(&self) -> io::Result<bool> {
        // SAFETY: all-zero is a valid initial representation for the waitid output record.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        loop {
            // SAFETY: `pid` names our direct, unreaped child; WNOWAIT observes its exit record
            // without releasing the PID that anchors the process-group identity.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                // SAFETY: successful waitid initialized `info`; zero means no exit event yet.
                return Ok(unsafe { info.si_pid() } != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn signal_process_group(&self, signal: i32) -> io::Result<()> {
        if self.reaped {
            return Err(io::Error::other(
                "refusing to signal an MCP process group after leader reap",
            ));
        }
        // SAFETY: `spawn_command` placed this owned child in a dedicated process group, and the
        // leader remains unreaped so the kernel cannot reuse its numeric PID as another PGID.
        if unsafe { libc::kill(-(self.pid as i32), signal) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(error)
    }

    async fn finish_stderr(&mut self, deadline: Instant) {
        if let Some(mut task) = self.stderr_task.take()
            && tokio::time::timeout_at(deadline, &mut task).await.is_err()
        {
            task.abort();
            let _ = task.await;
        }
    }

    fn diagnostics(&self) -> String {
        let stderr = self.stderr_lines.try_lock().map_or_else(
            |_| "<stderr unavailable>".to_string(),
            |lines| {
                let start = lines.len().saturating_sub(DIAGNOSTIC_TAIL_LINES);
                lines[start..].join("\n")
            },
        );
        format!("pid={}\nstderr tail:\n{stderr}", self.pid)
    }

    async fn send(&mut self, message: Value) -> Result<()> {
        let mut body = serde_json::to_vec(&message).context("serialize JSON-RPC message")?;
        body.push(b'\n');
        self.stdin
            .write_all(&body)
            .await
            .context("write JSON-RPC body")?;
        self.stdin.flush().await.context("flush MCP stdin")?;
        Ok(())
    }

    async fn read_response_message(&mut self, expected_id: u64) -> Result<Value> {
        loop {
            let mut line = String::new();
            let bytes = self
                .stdout
                .read_line(&mut line)
                .await
                .context("read JSON-RPC line")?;
            if bytes == 0 {
                bail!("unexpected EOF while reading JSON-RPC response");
            }
            let message: Value =
                serde_json::from_str(line.trim()).context("parse JSON-RPC response line")?;

            if let Some(method) = message.get("method").and_then(Value::as_str) {
                let request_id = message
                    .get("id")
                    .and_then(Value::as_u64)
                    .context("JSON-RPC request missing numeric id")?;
                match method {
                    "roots/list" => {
                        let roots = self
                            .roots
                            .iter()
                            .map(|uri| json!({ "uri": uri }))
                            .collect::<Vec<_>>();
                        self.send(json!({
                            "jsonrpc": "2.0",
                            "id": request_id,
                            "result": { "roots": roots },
                        }))
                        .await?;
                        continue;
                    }
                    _ => bail!("unexpected JSON-RPC request: {message}"),
                }
            }

            if message.get("id").is_none() {
                continue;
            }
            if message["id"].as_u64() != Some(expected_id) {
                bail!("unexpected JSON-RPC id: {message}");
            }
            return Ok(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn smoke_serializes_jsonrpc_line() {
        let value = json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}});
        let mut encoded = serde_json::to_vec(&value).expect("encode");
        encoded.push(b'\n');
        assert_eq!(encoded.last(), Some(&b'\n'));
    }
}
