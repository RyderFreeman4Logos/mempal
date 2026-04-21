//! JSON-RPC 2.0 client for `mempal serve --mcp` over stdio.

use std::collections::HashMap;
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

pub struct McpStdio {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    next_id: u64,
    roots: Vec<String>,
}

impl McpStdio {
    pub async fn start(db_path: &Path, extra_env: HashMap<String, String>) -> Result<Self> {
        let mempal_home = db_path
            .parent()
            .context("db_path must have a parent mempal home")?;
        let home = mempal_home.parent().unwrap_or(mempal_home);
        let config_path = mempal_home.join("config.toml");
        let embed_base_url = extra_env.get("MEMPAL_TEST_EMBED_BASE_URL").cloned();
        let config = if let Some(embed_base_url) = embed_base_url {
            format!(
                r#"
db_path = "{}"

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

[hooks]
enabled = true

[daemon]
log_path = "{}"
"#,
                db_path.display(),
                embed_base_url,
                embed_base_url,
                mempal_home.join("daemon.log").display()
            )
        } else {
            format!(
                r#"
db_path = "{}"

[embed]
backend = "model2vec"

[hooks]
enabled = true

[daemon]
log_path = "{}"
"#,
                db_path.display(),
                mempal_home.join("daemon.log").display()
            )
        };
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
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command.spawn().context("spawn mempal MCP child")?;
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
        })
    }

    pub async fn initialize(&mut self) -> Result<ServerInfo> {
        self.initialize_with_roots(&[]).await
    }

    pub async fn initialize_with_roots(&mut self, roots: &[&str]) -> Result<ServerInfo> {
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
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        self.read_response(id).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        let _ = self.call("shutdown", json!({})).await;
        let _ = self
            .send(json!({
                "jsonrpc": "2.0",
                "method": "notifications/exit"
            }))
            .await;
        let _ = tokio::time::timeout(Duration::from_secs(3), self.child.wait()).await;
        let _ = self.child.kill().await;
        if let Some(task) = self.stderr_task.take() {
            let _ = task.await;
        }
        Ok(())
    }

    pub async fn stderr_lines(&self) -> Vec<String> {
        self.stderr_lines.lock().await.clone()
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

    async fn read_response(&mut self, expected_id: u64) -> Result<Value> {
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
            if let Some(error) = message.get("error") {
                bail!("JSON-RPC error: {error}");
            }
            return Ok(message["result"].clone());
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
