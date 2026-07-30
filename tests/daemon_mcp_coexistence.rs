#![cfg(target_os = "linux")]

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use common::harness::{CapturedChild, McpStdio};
use mempal::core::async_db::RESOURCE_BOUNDED_READERS;
use mempal::core::db::Database;
use mempal::core::db_admission::{DbHolderClass, ProfileDbAdmission};
use mempal::daemon_recovery::{DaemonRecovery, MAX_RESTARTS_PER_WINDOW, RecoveryPhase};
use serde_json::{Value, json};
use tempfile::TempDir;

const SQLITE_WRITER_LEASE_NAME: &str = "sqlite-writer";
const ASYNC_DB_CONNECTION_CACHE_BYTES: u64 = 16 * 1024 * 1024;

struct TestHome {
    _tempdir: TempDir,
    home: PathBuf,
    mempal_home: PathBuf,
    db_path: PathBuf,
}

impl TestHome {
    fn new() -> Result<Self> {
        let tempdir = TempDir::new_in("/tmp").context("create test home")?;
        let home = tempdir.path().to_path_buf();
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).context("create mempal home")?;
        let db_path = mempal_home.join("palace.db");
        drop(Database::open(&db_path).context("initialize test database")?);
        fs::write(
            mempal_home.join("config.toml"),
            format!(
                r#"
db_path = "{}"

[embed]
backend = "stub"

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[daemon]
log_path = "{}"
"#,
                db_path.display(),
                mempal_home.join("daemon.log").display()
            ),
        )
        .context("write daemon config")?;
        Ok(Self {
            _tempdir: tempdir,
            home,
            mempal_home,
            db_path,
        })
    }

    fn spawn_daemon(&self, label: &str) -> Result<CapturedChild> {
        let runtime_dir = self.mempal_home.join("runtime");
        let mut command = Command::new(env!("CARGO_BIN_EXE_mempal"));
        command
            .args(["daemon", "--foreground"])
            .env("HOME", &self.home)
            .env(
                mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
                &runtime_dir,
            )
            .env_remove("MEMPAL_EMBED_BACKEND")
            .env_remove("MEMPAL_EMBED_BASE_URL")
            .env_remove("MEMPAL_EMBED_MODEL")
            .env_remove("MEMPAL_EMBED_DIM")
            .stdin(Stdio::null());
        CapturedChild::spawn(
            &mut command,
            &runtime_dir,
            label,
            Some(self.mempal_home.join("daemon.log")),
        )
        .context("spawn foreground daemon")
    }
}

async fn call_status(client: &mut McpStdio) -> Result<Value> {
    tokio::time::timeout(
        Duration::from_secs(15),
        client.call(
            "tools/call",
            json!({
                "name": "mempal_status",
                "arguments": {},
            }),
        ),
    )
    .await
    .context("mempal_status timed out")?
}

async fn wait_for_daemon_writer_lease(
    db_path: &Path,
    pid_path: &Path,
    timeout: Duration,
) -> Result<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        let pid = fs::read_to_string(pid_path)
            .ok()
            .and_then(|content| content.trim().parse::<u32>().ok());
        if let Some(pid) = pid {
            let db_path = db_path.to_path_buf();
            let leases = tokio::task::spawn_blocking(move || {
                let db = Database::open_query_only(&db_path)?;
                db.runtime_writer_lease_status_read_only(Some(SQLITE_WRITER_LEASE_NAME))
            })
            .await
            .context("join writer lease probe")??;
            if leases
                .iter()
                .any(|lease| lease.mode == "daemon" && lease.pid == pid)
            {
                return Ok(pid);
            }
        }
        if Instant::now() >= deadline {
            bail!("daemon did not publish a live writer lease before deadline");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop_daemon(daemon: &mut CapturedChild) -> Result<ExitStatus> {
    daemon
        .signal(libc::SIGTERM)
        .context("signal foreground daemon")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = daemon.try_wait().context("poll foreground daemon")? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            bail!("foreground daemon did not stop\n{}", daemon.diagnostics());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn assert_daemon_coexists_with_mcp_count(mcp_count: usize) -> Result<()> {
    let test_home = TestHome::new()?;
    let mut clients = Vec::with_capacity(mcp_count);
    for _ in 0..mcp_count {
        let mut client = McpStdio::start(&test_home.db_path, HashMap::new()).await?;
        client.initialize().await?;
        let status = call_status(&mut client).await?;
        assert!(status["structuredContent"].is_object());
        clients.push(client);
    }

    let before_daemon =
        ProfileDbAdmission::snapshot(&test_home.db_path).context("snapshot MCP holders")?;
    assert!(
        before_daemon.holders.iter().all(|holder| {
            holder.holder_class != DbHolderClass::Mcp
                || holder.connection_count != RESOURCE_BOUNDED_READERS + 1
                || holder.configured_cache_bytes
                    != ASYNC_DB_CONNECTION_CACHE_BYTES
                        .saturating_mul((RESOURCE_BOUNDED_READERS + 1) as u64)
        }),
        "healthy status MCPs must not own writer-capable pools: {:?}",
        before_daemon.holders
    );
    assert_eq!(
        before_daemon
            .holders
            .iter()
            .filter(|holder| {
                holder.holder_class == DbHolderClass::Mcp
                    && holder.connection_count == RESOURCE_BOUNDED_READERS
                    && holder.configured_cache_bytes
                        == ASYNC_DB_CONNECTION_CACHE_BYTES
                            .saturating_mul(RESOURCE_BOUNDED_READERS as u64)
            })
            .count(),
        mcp_count,
        "each MCP status process must own exactly one query-only pool"
    );

    let mut daemon = test_home.spawn_daemon(&format!("coexist-{mcp_count}-mcp"))?;
    let pid_path = test_home.mempal_home.join("daemon.pid");
    let daemon_pid =
        wait_for_daemon_writer_lease(&test_home.db_path, &pid_path, Duration::from_secs(10))
            .await
            .with_context(|| daemon.diagnostics())?;

    for client in &mut clients {
        let status = call_status(client).await?;
        assert!(
            status["structuredContent"]["database_diagnostic"].is_null(),
            "status degraded while daemon owned the writer lease: {status}"
        );
    }

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        daemon.try_wait().context("poll stable daemon")?.is_none(),
        "daemon exited after reaching readiness\n{}",
        daemon.diagnostics()
    );
    assert_eq!(
        fs::read_to_string(&pid_path)
            .context("read stable daemon pidfile")?
            .trim()
            .parse::<u32>()
            .context("parse stable daemon pid")?,
        daemon_pid
    );

    let recovery = DaemonRecovery::new(&test_home.mempal_home)
        .snapshot()
        .context("read daemon recovery state")?;
    assert_eq!(recovery.phase, RecoveryPhase::Healthy);
    assert_eq!(recovery.recent_fault_count, 0);
    assert_eq!(recovery.restart_budget_remaining, MAX_RESTARTS_PER_WINDOW);
    let holders = mempal::process_diagnostics::inspect_db_holders(&test_home.db_path);
    assert_eq!(holders.stale_mcp_server_count, 0, "{holders:?}");
    assert_eq!(holders.orphan_daemon_count, 0, "{holders:?}");

    let status = stop_daemon(&mut daemon).await?;
    assert!(
        status.success(),
        "foreground daemon shutdown failed: {status}\n{}",
        daemon.diagnostics()
    );
    for client in &mut clients {
        client.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_coexists_with_one_and_multiple_healthy_mcp_servers() -> Result<()> {
    assert_daemon_coexists_with_mcp_count(1).await?;
    assert_daemon_coexists_with_mcp_count(2).await
}

#[test]
fn daemon_refuses_a_live_incompatible_writer_lease_without_takeover() -> Result<()> {
    let test_home = TestHome::new()?;
    let lease_db = Database::open(&test_home.db_path).context("open writer lease fixture")?;
    let lease = lease_db
        .runtime_writer_lease_acquire_preserving_live_holders(
            SQLITE_WRITER_LEASE_NAME,
            "issue-849-incompatible-writer",
            "mcp-ingest-worker",
            300,
            None,
        )
        .context("acquire incompatible writer lease")?
        .context("writer lease should be available")?;

    let started = Instant::now();
    let mut daemon = test_home.spawn_daemon("incompatible-writer")?;
    let deadline = started + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = daemon.try_wait().context("poll rejected daemon")? {
            break status;
        }
        if Instant::now() >= deadline {
            bail!(
                "daemon did not reject the live writer lease\n{}",
                daemon.diagnostics()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    assert!(!status.success(), "incompatible writer must be refused");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        lease_db
            .runtime_writer_lease_is_active(&lease)
            .context("verify original writer lease")?,
        "daemon must not replace a live non-daemon writer lease"
    );
    assert!(
        !test_home.mempal_home.join("daemon.pid").exists(),
        "failed daemon must remove its pidfile"
    );
    let recovery = DaemonRecovery::new(&test_home.mempal_home)
        .snapshot()
        .context("read rejected daemon recovery state")?;
    assert_ne!(recovery.phase, RecoveryPhase::Cooldown);
    assert_eq!(recovery.recent_fault_count, 0);
    assert_eq!(recovery.restart_budget_remaining, MAX_RESTARTS_PER_WINDOW);
    assert!(recovery.last_fault.is_none());
    assert!(
        lease_db
            .runtime_writer_lease_release(&lease)
            .context("release incompatible writer lease")?
    );
    Ok(())
}
