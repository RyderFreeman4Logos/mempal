#![cfg(target_os = "linux")]

mod common;
#[path = "support/local_gate_child.rs"]
mod local_gate_child;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use common::harness::{CapturedChild, McpStdio};
use local_gate_child::{RecordedProcessIdentity, capture_recorded_process};
use mempal::core::async_db::RESOURCE_BOUNDED_READERS;
use mempal::core::config::Config;
use mempal::core::db::Database;
use mempal::core::db_admission::{DbHolderClass, ProfileDbAdmission};
use mempal::core::types::{Drawer, SourceType};
use mempal::daemon_bootstrap::DAEMON_TEMPORARY_ADMISSION_REFUSAL_EXIT_STATUS;
use mempal::daemon_recovery::{DaemonRecovery, MAX_RESTARTS_PER_WINDOW, RecoveryPhase};
use serde_json::{Value, json};
use tempfile::TempDir;

const SQLITE_WRITER_LEASE_NAME: &str = "sqlite-writer";
const ASYNC_DB_CONNECTION_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const COEXISTENCE_DRAWER_ID: &str = "issue-853-live-mcp";
const MCP_DESCENDANT_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const MCP_DESCENDANT_REAP_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
        let db = Database::open(&db_path).context("initialize test database")?;
        db.insert_drawer(&Drawer {
            id: COEXISTENCE_DRAWER_ID.to_string(),
            content: "same version MCP daemon bootstrap regression".to_string(),
            wing: "mempal".to_string(),
            room: Some("coexistence".to_string()),
            source_file: Some("issue-853.md".to_string()),
            source_type: SourceType::AgentInference,
            added_at: "2026-08-01T00:00:00Z".to_string(),
            ..Drawer::default()
        })
        .context("insert coexistence drawer")?;
        db.insert_vector(
            COEXISTENCE_DRAWER_ID,
            &vec![0.0; Config::default().embed.resolved_openai_dim()],
        )
        .context("insert coexistence vector")?;
        drop(db);
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

fn spawn_hostile_mcp(respond_to_initialize: bool, descendant_pid_path: &Path) -> Result<McpStdio> {
    let script = if respond_to_initialize {
        r#"trap '' TERM
sleep 60 &
descendant_pid="$!"
descendant_start="$(awk '{print $22}' "/proc/${descendant_pid}/stat")"
printf '%s %s\n' "${descendant_pid}" "${descendant_start}" > "$MEMPAL_DESCENDANT_PID_FILE"
printf 'hostile shutdown fixture\n' >&2
IFS= read -r _
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"hostile-fixture","version":"0.0.0"}}}'
while IFS= read -r _; do :; done"#
    } else {
        r#"trap '' TERM
sleep 60 &
descendant_pid="$!"
descendant_start="$(awk '{print $22}' "/proc/${descendant_pid}/stat")"
printf '%s %s\n' "${descendant_pid}" "${descendant_start}" > "$MEMPAL_DESCENDANT_PID_FILE"
printf 'hostile initialize fixture\n' >&2
while IFS= read -r _; do :; done"#
    };
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args(["-c", script])
        .env("MEMPAL_DESCENDANT_PID_FILE", descendant_pid_path);
    McpStdio::spawn_command(&mut command)
}

fn spawn_malformed_initialize_mcp(descendant_pid_path: &Path) -> Result<McpStdio> {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            r#"sleep 60 &
descendant_pid="$!"
descendant_start="$(awk '{print $22}' "/proc/${descendant_pid}/stat")"
printf '%s %s\n' "${descendant_pid}" "${descendant_start}" > "$MEMPAL_DESCENDANT_PID_FILE"
IFS= read -r _
printf '%s\n' '{'
while IFS= read -r _; do :; done"#,
        ])
        .env("MEMPAL_DESCENDANT_PID_FILE", descendant_pid_path);
    McpStdio::spawn_command(&mut command)
}

fn spawn_graceful_mcp_with_descendant(descendant_pid_path: &Path) -> Result<McpStdio> {
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .args([
            "-c",
            r#"sleep 60 &
descendant_pid="$!"
descendant_start="$(awk '{print $22}' "/proc/${descendant_pid}/stat")"
printf '%s %s\n' "${descendant_pid}" "${descendant_start}" > "$MEMPAL_DESCENDANT_PID_FILE"
IFS= read -r _
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"graceful-fixture","version":"0.0.0"}}}'
IFS= read -r _
IFS= read -r _
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{}}'
IFS= read -r _"#,
        ])
        .env("MEMPAL_DESCENDANT_PID_FILE", descendant_pid_path);
    McpStdio::spawn_command(&mut command)
}

fn read_recorded_process_identity(path: &Path) -> Result<RecordedProcessIdentity> {
    let record = fs::read_to_string(path)
        .with_context(|| format!("read descendant identity from {}", path.display()))?;
    let mut fields = record.split_ascii_whitespace();
    let identity = RecordedProcessIdentity {
        pid: fields
            .next()
            .context("missing descendant PID")?
            .parse()
            .context("parse descendant PID")?,
        start_time_ticks: fields
            .next()
            .context("missing descendant start time")?
            .parse()
            .context("parse descendant start time")?,
    };
    if fields.next().is_some() {
        bail!("descendant identity contains unexpected fields");
    }
    Ok(identity)
}

async fn assert_process_exited(pid_path: &Path) -> Result<()> {
    let identity = read_recorded_process_identity(pid_path)?;
    let Some(process) = capture_recorded_process(identity)? else {
        return Ok(());
    };
    let deadline = Instant::now() + MCP_DESCENDANT_REAP_TIMEOUT;
    while process.is_running()? && Instant::now() < deadline {
        tokio::time::sleep(MCP_DESCENDANT_REAP_POLL_INTERVAL).await;
    }
    if process.is_running()? {
        process.send_signal(libc::SIGKILL)?;
        bail!("MCP descendant {} survived lifecycle cleanup", identity.pid);
    }
    Ok(())
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
                Database::with_diagnostic_read_only(&db_path, |db| {
                    db.runtime_writer_lease_status_read_only(Some(SQLITE_WRITER_LEASE_NAME))
                })?
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

async fn assert_daemon_coexists_with_mcp_count(
    mcp_count: usize,
    open_writer_pool: bool,
) -> Result<()> {
    let test_home = TestHome::new()?;
    let mut clients = Vec::with_capacity(mcp_count);
    for _ in 0..mcp_count {
        let mut client = McpStdio::start(&test_home.db_path, HashMap::new()).await?;
        client.initialize().await?;
        let status = call_status(&mut client).await?;
        assert!(status["structuredContent"].is_object());
        if open_writer_pool {
            let deleted = client
                .call(
                    "tools/call",
                    json!({
                        "name": "mempal_delete",
                        "arguments": {"drawer_id": "issue-853-missing-drawer"},
                    }),
                )
                .await?;
            assert_eq!(
                deleted["structuredContent"]["deleted"].as_bool(),
                Some(false)
            );
        }
        clients.push(client);
    }

    let before_daemon =
        ProfileDbAdmission::snapshot(&test_home.db_path).context("snapshot MCP holders")?;
    assert_eq!(
        before_daemon
            .holders
            .iter()
            .filter(|holder| {
                holder.holder_class == DbHolderClass::Mcp
                    && holder.connection_count == RESOURCE_BOUNDED_READERS + 1
                    && holder.configured_cache_bytes
                        == ASYNC_DB_CONNECTION_CACHE_BYTES
                            .saturating_mul((RESOURCE_BOUNDED_READERS + 1) as u64)
            })
            .count(),
        usize::from(open_writer_pool) * mcp_count,
        "unexpected MCP writer-capable pool count"
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
        let read = client
            .call(
                "tools/call",
                json!({
                    "name": "mempal_read_drawer",
                    "arguments": {"drawer_id": COEXISTENCE_DRAWER_ID},
                }),
            )
            .await?;
        assert_eq!(
            read["structuredContent"]["drawer_id"].as_str(),
            Some(COEXISTENCE_DRAWER_ID)
        );
        let search = client
            .call(
                "tools/call",
                json!({
                    "name": "mempal_search",
                    "arguments": {
                        "query": "bootstrap",
                        "top_k": 5,
                    },
                }),
            )
            .await?;
        assert!(
            search["structuredContent"]["results"]
                .as_array()
                .is_some_and(|results| results
                    .iter()
                    .any(|result| { result["drawer_id"].as_str() == Some(COEXISTENCE_DRAWER_ID) })),
            "search failed while daemon owned the writer lease: {search}"
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
async fn daemon_coexists_with_one_and_two_writer_capable_mcp_servers() -> Result<()> {
    assert_daemon_coexists_with_mcp_count(1, true).await?;
    assert_daemon_coexists_with_mcp_count(2, true).await
}

#[tokio::test]
async fn mcp_lifecycle_timeouts_reap_hostile_children() -> Result<()> {
    let tempdir = TempDir::new_in("/tmp").context("create hostile MCP test directory")?;
    let initialize_descendant = tempdir.path().join("initialize-descendant.pid");
    let mut initializing = spawn_hostile_mcp(false, &initialize_descendant)?;
    let initializing_pid = initializing.id();
    let started = Instant::now();
    let initialize_error = initializing
        .initialize()
        .await
        .expect_err("hostile child must time out during initialize");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        initializing.is_reaped(),
        "child {initializing_pid} not reaped"
    );
    let initialize_diagnostic = format!("{initialize_error:#}");
    assert!(initialize_diagnostic.contains("MCP initialize timed out"));
    assert!(initialize_diagnostic.contains("hostile initialize fixture"));
    assert_process_exited(&initialize_descendant).await?;

    let shutdown_descendant = tempdir.path().join("shutdown-descendant.pid");
    let mut shutting_down = spawn_hostile_mcp(true, &shutdown_descendant)?;
    shutting_down.initialize().await?;
    let shutting_down_pid = shutting_down.id();
    let started = Instant::now();
    let shutdown_error = shutting_down
        .shutdown()
        .await
        .expect_err("hostile child must time out during shutdown");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        shutting_down.is_reaped(),
        "child {shutting_down_pid} not reaped"
    );
    let shutdown_diagnostic = format!("{shutdown_error:#}");
    assert!(shutdown_diagnostic.contains("MCP shutdown response timed out"));
    assert!(shutdown_diagnostic.contains("hostile shutdown fixture"));
    assert_process_exited(&shutdown_descendant).await?;
    Ok(())
}

#[tokio::test]
async fn mcp_drop_fences_descendant_after_malformed_initialize() -> Result<()> {
    let tempdir = TempDir::new_in("/tmp").context("create malformed MCP test directory")?;
    let descendant = tempdir.path().join("malformed-initialize-descendant.pid");
    let mut client = spawn_malformed_initialize_mcp(&descendant)?;

    client
        .initialize()
        .await
        .expect_err("malformed initialize response must fail");
    assert!(
        !client.is_reaped(),
        "early initialize error must use Drop fallback"
    );
    drop(client);

    assert_process_exited(&descendant).await
}

#[tokio::test]
async fn mcp_graceful_shutdown_fences_surviving_descendant_before_reap() -> Result<()> {
    let tempdir = TempDir::new_in("/tmp").context("create graceful MCP test directory")?;
    let descendant = tempdir.path().join("graceful-descendant.pid");
    let mut client = spawn_graceful_mcp_with_descendant(&descendant)?;

    client.initialize().await?;
    client.shutdown().await?;

    assert!(client.is_reaped(), "graceful MCP leader was not reaped");
    assert_process_exited(&descendant).await
}

#[tokio::test]
async fn process_exit_check_never_signals_reused_pid() -> Result<()> {
    let tempdir = TempDir::new_in("/tmp").context("create reused PID test directory")?;
    let identity_path = tempdir.path().join("reused.identity");
    let pid = std::process::id() as i32;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat.rsplit_once(") ").context("current process stat")?;
    let actual_start = fields
        .split_ascii_whitespace()
        .nth(19)
        .context("current process start time")?
        .parse::<u64>()?;
    let stale_start = actual_start.checked_add(1).context("start time headroom")?;
    fs::write(&identity_path, format!("{pid} {stale_start}\n"))?;

    assert_process_exited(&identity_path).await?;

    let current = capture_recorded_process(RecordedProcessIdentity {
        pid,
        start_time_ticks: actual_start,
    })?
    .context("capture current process by exact identity")?;
    assert!(
        current.is_running()?,
        "mismatched start time must not authorize signaling the reused PID"
    );
    Ok(())
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

    assert_eq!(
        status.code(),
        Some(DAEMON_TEMPORARY_ADMISSION_REFUSAL_EXIT_STATUS),
        "a live incompatible writer lease must be a temporary refusal"
    );
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
