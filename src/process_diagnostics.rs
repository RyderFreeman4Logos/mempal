use std::path::Path;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::{
    fs,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use crate::db_path_identity::{DbFileIdentity, db_file_targets};
use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
const DB_HOLDER_INSPECTION_TIMEOUT: &str =
    "DB holder inspection exceeded time budget; results may be incomplete";

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy)]
struct DbHolderInspectionDeadline {
    expires_at: Option<Instant>,
    #[cfg(test)]
    fd_entries_remaining: Option<usize>,
}

#[cfg(target_os = "linux")]
impl DbHolderInspectionDeadline {
    fn none() -> Self {
        Self {
            expires_at: None,
            #[cfg(test)]
            fd_entries_remaining: None,
        }
    }

    fn at(expires_at: Instant) -> Self {
        Self {
            expires_at: Some(expires_at),
            #[cfg(test)]
            fd_entries_remaining: None,
        }
    }

    #[cfg(test)]
    fn after_fd_entries(fd_entries_remaining: usize) -> Self {
        Self {
            expires_at: None,
            fd_entries_remaining: Some(fd_entries_remaining),
        }
    }

    fn expired(&self) -> bool {
        self.expires_at
            .is_some_and(|expires_at| Instant::now() >= expires_at)
    }

    fn consume_fd_entry_budget(&mut self) -> bool {
        if self.expired() {
            return true;
        }

        #[cfg(test)]
        if let Some(remaining) = self.fd_entries_remaining.as_mut() {
            if *remaining == 0 {
                return true;
            }
            *remaining -= 1;
        }

        false
    }
}

#[cfg(target_os = "linux")]
enum OpenedDbFilesScan {
    Complete(Vec<String>),
    DeadlineExceeded,
}

/// Best-effort report of live processes that currently hold the mempal SQLite
/// database, WAL, or shared-memory file open.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DbHolderReport {
    pub db_path: String,
    pub holder_count: usize,
    pub extra_holder_count: usize,
    pub stale_mcp_server_count: usize,
    pub orphan_daemon_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub holders: Vec<DbHolderProcess>,
}

impl DbHolderReport {
    pub fn has_problem(&self) -> bool {
        self.extra_holder_count > 0
            || self.stale_mcp_server_count > 0
            || self.orphan_daemon_count > 0
            || self.error.is_some()
    }
}

/// Conservative remediation plan for stale mempal-owned DB holders.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbHolderRemediationPlan {
    /// Holder identities that are safe to terminate automatically if they
    /// still match immediately before signalling.
    pub terminate_targets: Vec<DbHolderRemediationTarget>,
    /// PIDs that are safe to terminate automatically.
    ///
    /// This is a convenience projection for diagnostics and tests. Signal
    /// paths must use `terminate_targets` so PID reuse is guarded by identity.
    pub terminate_pids: Vec<i32>,
    /// Holders that must remain under operator control.
    pub manual_holders: Vec<DbHolderProcess>,
}

/// Immutable identity of a stale mempal-owned DB holder selected for cleanup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbHolderRemediationTarget {
    pub pid: i32,
    pub role: String,
    pub classification: String,
    pub opened_files: Vec<String>,
    pub started_at_unix_secs: Option<u64>,
}

impl DbHolderRemediationTarget {
    pub fn from_holder(holder: &DbHolderProcess) -> Self {
        Self {
            pid: holder.pid,
            role: holder.role.clone(),
            classification: holder.classification.clone(),
            opened_files: holder.opened_files.clone(),
            started_at_unix_secs: holder.started_at_unix_secs,
        }
    }

    pub fn matches_holder(&self, holder: &DbHolderProcess) -> bool {
        self.pid == holder.pid
            && self.role == holder.role
            && self.classification == holder.classification
            && matches!(
                self.classification.as_str(),
                "stale_mcp_server" | "orphan_daemon"
            )
            && !holder.current_process
            && !holder.current_daemon
            && !holder.current_mcp_server
            && self.matches_start_time(holder)
            && self
                .opened_files
                .iter()
                .all(|expected| holder.opened_files.contains(expected))
    }

    fn matches_start_time(&self, holder: &DbHolderProcess) -> bool {
        match (self.started_at_unix_secs, holder.started_at_unix_secs) {
            (Some(expected), Some(actual)) => expected == actual,
            (None, None) => true,
            _ => false,
        }
    }

    pub fn describe(&self) -> String {
        let started = self
            .started_at_unix_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        format!(
            "pid={} role={} classification={} started_at={} files={}",
            self.pid,
            self.role,
            self.classification,
            started,
            self.opened_files.join(",")
        )
    }
}

/// Plan cleanup for stale mempal-owned holders already proven to hold this DB.
///
/// The report is keyed to one SQLite DB identity, so this function only decides
/// which roles are safe to remediate. It never selects `extra_holder`,
/// `current_daemon`, `current_mcp_server`, or `current_process` entries.
pub fn plan_stale_db_holder_remediation(report: &DbHolderReport) -> DbHolderRemediationPlan {
    let mut terminate_targets = Vec::new();
    let mut manual_holders = Vec::new();

    for holder in &report.holders {
        match holder.classification.as_str() {
            "stale_mcp_server" | "orphan_daemon"
                if !holder.current_process
                    && !holder.current_daemon
                    && !holder.current_mcp_server =>
            {
                terminate_targets.push(DbHolderRemediationTarget::from_holder(holder));
            }
            _ => manual_holders.push(holder.clone()),
        }
    }

    terminate_targets.sort_by_key(|target| target.pid);
    terminate_targets.dedup_by_key(|target| target.pid);
    let terminate_pids = terminate_targets
        .iter()
        .map(|target| target.pid)
        .collect::<Vec<_>>();
    manual_holders.sort_by_key(|holder| holder.pid);

    DbHolderRemediationPlan {
        terminate_targets,
        terminate_pids,
        manual_holders,
    }
}

/// Build an actionable daemon-startup lock diagnostic without exposing argv.
pub fn format_db_lock_remediation_hint(
    db_path: &Path,
    error: &str,
    report: &DbHolderReport,
    terminated_pids: &[i32],
    remediation_errors: &[String],
) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "failed to open daemon database {}: {error}",
        db_path.display()
    ));

    if !terminated_pids.is_empty() {
        lines.push(format!(
            "automatically terminated stale mempal-owned DB holders: {}",
            format_pid_list(terminated_pids)
        ));
    }
    if !remediation_errors.is_empty() {
        lines.push(format!(
            "automatic cleanup encountered errors: {}",
            remediation_errors.join("; ")
        ));
    }
    if report.holders.is_empty() {
        lines.push(
            "no live DB holders were visible in process diagnostics after cleanup".to_string(),
        );
    } else {
        lines.push("live DB holders after cleanup:".to_string());
        for holder in &report.holders {
            lines.push(format!(
                "- pid={} role={} classification={} files={} command={}",
                holder.pid,
                holder.role,
                holder.classification,
                holder.opened_files.join(","),
                holder.command
            ));
        }
    }

    lines.push(
        "remediation: mempal only auto-terminates stale_mcp_server and orphan_daemon holders for \
         this exact DB; stop extra/current holder processes manually or run `mempal daemon status` \
         before retrying daemon startup"
            .to_string(),
    );
    lines.join("\n")
}

/// Summarize live SQLite holders without exposing argv or payload content.
pub fn format_db_holder_role_summary(report: &DbHolderReport) -> String {
    if let Some(error) = report.error.as_deref() {
        return format!("holder inspection failed: {error}");
    }
    if report.holders.is_empty() {
        return "no live DB holders visible".to_string();
    }

    report
        .holders
        .iter()
        .map(|holder| {
            format!(
                "pid={} role={} classification={} files={}",
                holder.pid,
                holder.role,
                holder.classification,
                holder.opened_files.join(",")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Return the safe next operator step for a busy SQLite diagnostic.
pub fn sqlite_lock_safe_next_step(report: &DbHolderReport) -> &'static str {
    if report.stale_mcp_server_count > 0 || report.orphan_daemon_count > 0 {
        return "run `mempal doctor` or `mempal daemon status` to inspect stale mempal-owned holders before retrying";
    }
    if report.extra_holder_count > 0 {
        return "stop or wait for the extra process holding palace.db, then retry";
    }
    if report
        .holders
        .iter()
        .any(|holder| holder.current_mcp_server || holder.classification == "current_mcp_server")
    {
        return "the current MCP server is the visible holder; wait for the active write to finish and retry without killing the server";
    }
    if report
        .holders
        .iter()
        .any(|holder| holder.current_daemon || holder.classification == "current_daemon")
    {
        return "the current daemon is the visible holder; wait for the active queued write to finish and retry";
    }
    "wait for the transient SQLite writer to finish, then retry"
}

fn format_pid_list(pids: &[i32]) -> String {
    pids.iter()
        .map(i32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// One live process with an open fd to `palace.db`, `palace.db-wal`, or
/// `palace.db-shm`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DbHolderProcess {
    pub pid: i32,
    pub role: String,
    pub classification: String,
    /// Sanitized process label for human diagnostics. This is never raw argv.
    pub command: String,
    pub opened_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at_unix_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    pub current_process: bool,
    pub current_daemon: bool,
    pub current_mcp_server: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ProcessMemoryReport {
    pub pid: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vm_hwm_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_dirty_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anonymous_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub swap_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_read_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_write_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_cancelled_write_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe_path: Option<String>,
    pub exe_deleted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Inspect live DB holders. On non-Linux platforms this returns an empty report
/// because `/proc/<pid>/fd` enumeration is Linux-specific.
pub fn inspect_db_holders(db_path: &Path) -> DbHolderReport {
    #[cfg(target_os = "linux")]
    {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        inspect_db_holders_in_proc(
            db_path,
            Path::new("/proc"),
            now_secs,
            clock_ticks_per_second(),
        )
    }

    #[cfg(not(target_os = "linux"))]
    {
        empty_report(db_path, None)
    }
}

/// Inspect live DB holders with a wall-clock budget.
///
/// When `/proc` enumeration is slow, the returned report includes any holders
/// found before the budget expired and an `error` explaining that the result is
/// incomplete. This keeps health-oriented commands responsive while preserving
/// the stricter unbounded scan for remediation paths.
pub fn inspect_db_holders_bounded(db_path: &Path, max_duration: Duration) -> DbHolderReport {
    #[cfg(target_os = "linux")]
    {
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        inspect_db_holders_in_proc_with_deadline(
            db_path,
            Path::new("/proc"),
            now_secs,
            clock_ticks_per_second(),
            Instant::now() + max_duration,
        )
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = max_duration;
        empty_report(db_path, None)
    }
}

pub fn inspect_process_memory(pid: i32) -> ProcessMemoryReport {
    #[cfg(target_os = "linux")]
    {
        inspect_process_memory_in_proc(pid, Path::new("/proc"))
    }

    #[cfg(not(target_os = "linux"))]
    {
        ProcessMemoryReport {
            pid,
            error: Some("process memory diagnostics are only available on Linux".to_string()),
            ..ProcessMemoryReport::default()
        }
    }
}

/// Inspect live DB holders for daemon-startup remediation after the singleton
/// daemon lock has already been acquired.
///
/// A process named by `daemon.pid` is protected in status output, but once the
/// new daemon owns `daemon.lock`, a pidfile-only daemon holder is stale and can
/// be classified as an `orphan_daemon` for the cleanup path.
#[cfg(target_os = "linux")]
pub(crate) fn inspect_db_holders_for_startup_remediation(db_path: &Path) -> DbHolderReport {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    inspect_db_holders_in_proc_for_startup_remediation(
        db_path,
        Path::new("/proc"),
        now_secs,
        clock_ticks_per_second(),
    )
}

#[cfg(target_os = "linux")]
fn inspect_db_holders_in_proc(
    db_path: &Path,
    proc_root: &Path,
    now_secs: u64,
    clock_ticks_per_second: u64,
) -> DbHolderReport {
    inspect_db_holders_in_proc_with_daemon_pid_protection(
        db_path,
        proc_root,
        now_secs,
        clock_ticks_per_second,
        true,
        DbHolderInspectionDeadline::none(),
    )
}

#[cfg(target_os = "linux")]
fn inspect_db_holders_in_proc_with_deadline(
    db_path: &Path,
    proc_root: &Path,
    now_secs: u64,
    clock_ticks_per_second: u64,
    deadline: Instant,
) -> DbHolderReport {
    inspect_db_holders_in_proc_with_daemon_pid_protection(
        db_path,
        proc_root,
        now_secs,
        clock_ticks_per_second,
        true,
        DbHolderInspectionDeadline::at(deadline),
    )
}

#[cfg(target_os = "linux")]
fn inspect_db_holders_in_proc_for_startup_remediation(
    db_path: &Path,
    proc_root: &Path,
    now_secs: u64,
    clock_ticks_per_second: u64,
) -> DbHolderReport {
    inspect_db_holders_in_proc_with_daemon_pid_protection(
        db_path,
        proc_root,
        now_secs,
        clock_ticks_per_second,
        false,
        DbHolderInspectionDeadline::none(),
    )
}

#[cfg(target_os = "linux")]
fn inspect_db_holders_in_proc_with_daemon_pid_protection(
    db_path: &Path,
    proc_root: &Path,
    now_secs: u64,
    clock_ticks_per_second: u64,
    protect_daemon_pid: bool,
    mut deadline: DbHolderInspectionDeadline,
) -> DbHolderReport {
    let current_pid = std::process::id() as i32;
    let daemon_pid = read_daemon_pid(db_path);
    let binary_name =
        crate::daemon_singleton::current_binary_name().unwrap_or_else(|| "mempal".to_string());
    let boot_time = read_boot_time(proc_root);
    let targets = db_file_targets(db_path);
    let entries = match fs::read_dir(proc_root) {
        Ok(entries) => entries,
        Err(error) => {
            return empty_report(
                db_path,
                Some(format!(
                    "failed to read process table {}: {error}",
                    proc_root.display()
                )),
            );
        }
    };

    let mut holders = Vec::new();
    for entry in entries.flatten() {
        if deadline.expired() {
            return build_timeout_report(db_path, holders);
        }
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(parse_pid) else {
            continue;
        };
        let pid_dir = entry.path();
        let opened_files = match opened_db_files(&pid_dir.join("fd"), &targets, &mut deadline) {
            OpenedDbFilesScan::Complete(opened_files) => opened_files,
            OpenedDbFilesScan::DeadlineExceeded => {
                return build_timeout_report(db_path, holders);
            }
        };
        if deadline.expired() {
            return build_timeout_report(db_path, holders);
        }
        if opened_files.is_empty() {
            continue;
        }

        let argv = read_cmdline(&pid_dir.join("cmdline"));
        let role = classify_role(&argv, &binary_name);
        let current_process = pid == current_pid;
        let current_daemon =
            protect_daemon_pid && daemon_pid == Some(pid) && role == "mempal_daemon";
        let stat_path = pid_dir.join("stat");
        let parent_pid = read_parent_pid(&stat_path);
        let current_mcp_server = current_process && role == "mempal_mcp_server";
        let orphan_mcp_server = role == "mempal_mcp_server" && parent_pid == Some(1);
        let classification = classify_holder(
            role,
            current_process,
            current_daemon,
            current_mcp_server,
            orphan_mcp_server,
        );
        let started_at_unix_secs = boot_time.and_then(|btime| {
            read_start_ticks(&stat_path).map(|ticks| {
                btime.saturating_add(ticks.saturating_div(clock_ticks_per_second.max(1)))
            })
        });
        let age_secs = started_at_unix_secs.map(|started| now_secs.saturating_sub(started));

        holders.push(DbHolderProcess {
            pid,
            role: role.to_string(),
            classification: classification.to_string(),
            command: command_display(role),
            opened_files,
            started_at_unix_secs,
            age_secs,
            current_process,
            current_daemon,
            current_mcp_server,
        });
    }

    holders.sort_by_key(|holder| holder.pid);
    build_report(db_path, holders, None)
}

#[cfg(target_os = "linux")]
fn build_timeout_report(db_path: &Path, holders: Vec<DbHolderProcess>) -> DbHolderReport {
    build_report(
        db_path,
        holders,
        Some(DB_HOLDER_INSPECTION_TIMEOUT.to_string()),
    )
}

fn build_report(
    db_path: &Path,
    holders: Vec<DbHolderProcess>,
    error: Option<String>,
) -> DbHolderReport {
    let stale_mcp_server_count = holders
        .iter()
        .filter(|holder| holder.classification == "stale_mcp_server")
        .count();
    let orphan_daemon_count = holders
        .iter()
        .filter(|holder| holder.classification == "orphan_daemon")
        .count();
    let extra_holder_count = holders
        .iter()
        .filter(|holder| holder.classification == "extra_holder")
        .count();

    DbHolderReport {
        db_path: db_path.display().to_string(),
        holder_count: holders.len(),
        extra_holder_count,
        stale_mcp_server_count,
        orphan_daemon_count,
        error,
        holders,
    }
}

fn empty_report(db_path: &Path, error: Option<String>) -> DbHolderReport {
    build_report(db_path, Vec::new(), error)
}

#[cfg(target_os = "linux")]
fn opened_db_files(
    fd_dir: &Path,
    targets: &[(&'static str, DbFileIdentity)],
    deadline: &mut DbHolderInspectionDeadline,
) -> OpenedDbFilesScan {
    let entries = match fs::read_dir(fd_dir) {
        Ok(entries) => entries,
        Err(_) => return OpenedDbFilesScan::Complete(Vec::new()),
    };
    let mut opened = Vec::new();
    for entry in entries.flatten() {
        if deadline.consume_fd_entry_budget() {
            return OpenedDbFilesScan::DeadlineExceeded;
        }
        let fd_path = entry.path();
        let Ok(target) = fs::read_link(&fd_path) else {
            continue;
        };
        for (kind, expected) in targets {
            if opened.iter().any(|opened_kind| opened_kind == kind) {
                continue;
            }
            if expected.matches_fd(&fd_path, &target) {
                opened.push((*kind).to_string());
            }
        }
    }
    opened.sort();
    OpenedDbFilesScan::Complete(opened)
}

#[cfg(target_os = "linux")]
fn read_daemon_pid(db_path: &Path) -> Option<i32> {
    let pid_path = daemon_pid_path(db_path);
    fs::read_to_string(pid_path)
        .ok()
        .and_then(|content| content.trim().parse::<i32>().ok())
}

#[cfg(target_os = "linux")]
fn daemon_pid_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join("daemon.pid")
}

#[cfg(target_os = "linux")]
fn read_cmdline(path: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    parse_cmdline(&bytes)
}

#[cfg(target_os = "linux")]
fn inspect_process_memory_in_proc(pid: i32, proc_root: &Path) -> ProcessMemoryReport {
    let pid_dir = proc_root.join(pid.to_string());
    if !pid_dir.exists() {
        return ProcessMemoryReport {
            pid,
            error: Some(format!(
                "process {pid} is not visible in {}",
                proc_root.display()
            )),
            ..ProcessMemoryReport::default()
        };
    }

    let mut report = ProcessMemoryReport {
        pid,
        ..ProcessMemoryReport::default()
    };
    match fs::read_link(pid_dir.join("exe")) {
        Ok(exe_path) => {
            let display = exe_path.to_string_lossy().into_owned();
            report.exe_deleted = display.ends_with(" (deleted)");
            report.exe_path = Some(display);
        }
        Err(error) => {
            report.error = Some(format!("failed to read process exe link: {error}"));
        }
    }
    if let Ok(status) = fs::read_to_string(pid_dir.join("status")) {
        report.rss_bytes = parse_proc_kb_metric(&status, "VmRSS:");
        report.vm_hwm_bytes = parse_proc_kb_metric(&status, "VmHWM:");
    }
    if let Ok(smaps) = fs::read_to_string(pid_dir.join("smaps_rollup")) {
        report.pss_bytes = parse_proc_kb_metric(&smaps, "Pss:");
        report.private_dirty_bytes = parse_proc_kb_metric(&smaps, "Private_Dirty:");
        report.anonymous_bytes = parse_proc_kb_metric(&smaps, "Anonymous:");
        report.swap_bytes = parse_proc_kb_metric(&smaps, "Swap:");
    }
    if let Ok(io) = fs::read_to_string(pid_dir.join("io")) {
        report.io_read_bytes = parse_proc_byte_metric(&io, "read_bytes:");
        report.io_write_bytes = parse_proc_byte_metric(&io, "write_bytes:");
        report.io_cancelled_write_bytes = parse_proc_byte_metric(&io, "cancelled_write_bytes:");
    }
    report
}

#[cfg(target_os = "linux")]
fn parse_proc_kb_metric(contents: &str, label: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let value = line.strip_prefix(label)?.trim();
        let kb = value.split_ascii_whitespace().next()?.parse::<u64>().ok()?;
        kb.checked_mul(1024)
    })
}

#[cfg(target_os = "linux")]
fn parse_proc_byte_metric(contents: &str, label: &str) -> Option<u64> {
    contents.lines().find_map(|line| {
        let value = line.strip_prefix(label)?.trim();
        value.split_ascii_whitespace().next()?.parse::<u64>().ok()
    })
}

#[cfg(target_os = "linux")]
fn parse_cmdline(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|byte| *byte == b'\0')
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn classify_role(argv: &[String], binary_name: &str) -> &'static str {
    if !is_mempal_invocation(argv, binary_name) {
        return "other";
    }
    if crate::daemon_singleton::is_daemon_argv(argv, binary_name)
        || crate::daemon_singleton::is_daemon_argv(argv, "mempal")
    {
        return "mempal_daemon";
    }
    if is_mcp_server_argv(argv) {
        return "mempal_mcp_server";
    }
    if is_read_only_cli_argv(argv) {
        return "mempal_readonly_cli";
    }
    "mempal_cli"
}

#[cfg(target_os = "linux")]
fn is_read_only_cli_argv(argv: &[String]) -> bool {
    let Some((_, subcommand)) = crate::daemon_singleton::first_cli_subcommand(argv) else {
        return false;
    };
    // Diagnostic-only classification for short-lived CLI readers that can overlap
    // MCP write admission. Keep this conservative: write-capable subcommands must
    // remain `mempal_cli` so busy diagnostics still fail fast on external writers.
    matches!(
        subcommand,
        "status"
            | "search"
            | "context"
            | "wake-up"
            | "tail"
            | "timeline"
            | "stats"
            | "view"
            | "reflect"
            | "field-taxonomy"
    )
}

#[cfg(target_os = "linux")]
fn classify_holder(
    role: &str,
    current_process: bool,
    current_daemon: bool,
    current_mcp_server: bool,
    orphan_mcp_server: bool,
) -> &'static str {
    if current_daemon {
        "current_daemon"
    } else if current_mcp_server {
        "current_mcp_server"
    } else if current_process {
        "current_process"
    } else if orphan_mcp_server {
        "stale_mcp_server"
    } else if role == "mempal_daemon" {
        "orphan_daemon"
    } else {
        "extra_holder"
    }
}

#[cfg(target_os = "linux")]
fn is_mempal_invocation(argv: &[String], binary_name: &str) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    let Some(name) = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    name == binary_name || name == "mempal"
}

#[cfg(target_os = "linux")]
fn is_mcp_server_argv(argv: &[String]) -> bool {
    let Some((_, subcommand)) = crate::daemon_singleton::first_cli_subcommand(argv) else {
        return false;
    };
    subcommand == "serve"
}

#[cfg(target_os = "linux")]
fn command_display(role: &str) -> String {
    match role {
        "mempal_daemon" => "mempal daemon".to_string(),
        "mempal_mcp_server" => "mempal serve".to_string(),
        "mempal_readonly_cli" => "mempal read-only cli".to_string(),
        "mempal_cli" => "mempal cli".to_string(),
        "other" => "other process".to_string(),
        _ => "<unknown>".to_string(),
    }
}

#[cfg(target_os = "linux")]
fn read_boot_time(proc_root: &Path) -> Option<u64> {
    let content = fs::read_to_string(proc_root.join("stat")).ok()?;
    content.lines().find_map(|line| {
        let value = line.strip_prefix("btime ")?;
        value.trim().parse::<u64>().ok()
    })
}

#[cfg(target_os = "linux")]
fn read_start_ticks(path: &Path) -> Option<u64> {
    let content = fs::read(path).ok()?;
    parse_start_ticks(&content)
}

#[cfg(target_os = "linux")]
fn read_parent_pid(path: &Path) -> Option<i32> {
    let content = fs::read(path).ok()?;
    parse_parent_pid(&content)
}

#[cfg(target_os = "linux")]
fn parse_parent_pid(stat: &[u8]) -> Option<i32> {
    let close = stat.windows(2).rposition(|window| window == b") ")?;
    let parent_pid = stat[close + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .nth(1)?;
    std::str::from_utf8(parent_pid).ok()?.parse::<i32>().ok()
}

#[cfg(target_os = "linux")]
fn parse_start_ticks(stat: &[u8]) -> Option<u64> {
    let close = stat.windows(2).rposition(|window| window == b") ")?;
    let start_ticks = stat[close + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty())
        .nth(19)?
        .iter()
        .try_fold(0_u64, |value, digit| {
            digit
                .checked_sub(b'0')
                .filter(|digit| *digit <= 9)
                .and_then(|digit| {
                    value
                        .checked_mul(10)
                        .and_then(|value| value.checked_add(u64::from(digit)))
                })
        })?;
    Some(start_ticks)
}

#[cfg(target_os = "linux")]
fn parse_pid(value: &str) -> Option<i32> {
    if value.is_empty() {
        return None;
    }
    value.parse::<i32>().ok().filter(|pid| *pid > 0)
}

#[cfg(target_os = "linux")]
fn clock_ticks_per_second() -> u64 {
    // SAFETY: sysconf(_SC_CLK_TCK) is a read-only libc query with no pointer
    // arguments and no Rust aliasing or lifetime requirements.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u64::try_from(ticks)
        .ok()
        .filter(|ticks| *ticks > 0)
        .unwrap_or(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(pid: i32, role: &str, classification: &str) -> DbHolderProcess {
        holder_with_start(pid, role, classification, None)
    }

    fn holder_with_start(
        pid: i32,
        role: &str,
        classification: &str,
        started_at_unix_secs: Option<u64>,
    ) -> DbHolderProcess {
        DbHolderProcess {
            pid,
            role: role.to_string(),
            classification: classification.to_string(),
            command: match role {
                "mempal_mcp_server" => "mempal serve".to_string(),
                "mempal_daemon" => "mempal daemon".to_string(),
                _ => "other process".to_string(),
            },
            opened_files: vec!["db".to_string()],
            started_at_unix_secs,
            age_secs: None,
            current_process: classification == "current_process",
            current_daemon: classification == "current_daemon",
            current_mcp_server: classification == "current_mcp_server",
        }
    }

    #[test]
    fn test_plan_stale_db_holder_remediation_only_targets_stale_mempal_roles() {
        let report = build_report(
            Path::new("/tmp/palace.db"),
            vec![
                holder(11, "mempal_mcp_server", "stale_mcp_server"),
                holder(22, "mempal_daemon", "orphan_daemon"),
                holder(33, "other", "extra_holder"),
                holder(44, "mempal_daemon", "current_daemon"),
                holder(55, "mempal_mcp_server", "current_mcp_server"),
            ],
            None,
        );

        let plan = plan_stale_db_holder_remediation(&report);

        assert_eq!(plan.terminate_pids, vec![11, 22]);
        assert_eq!(
            plan.terminate_targets
                .iter()
                .map(|target| target.pid)
                .collect::<Vec<_>>(),
            vec![11, 22]
        );
        assert_eq!(
            plan.manual_holders
                .iter()
                .map(|holder| holder.pid)
                .collect::<Vec<_>>(),
            vec![33, 44, 55]
        );
    }

    #[test]
    fn test_classify_role_distinguishes_read_only_cli_from_write_cli() {
        let status_argv = vec!["mempal".to_string(), "status".to_string()];
        assert_eq!(classify_role(&status_argv, "mempal"), "mempal_readonly_cli");

        let ingest_argv = vec!["mempal".to_string(), "ingest".to_string()];
        assert_eq!(classify_role(&ingest_argv, "mempal"), "mempal_cli");
    }

    #[test]
    fn test_db_holder_remediation_target_rejects_pid_reuse_identity_mismatch() {
        let target = DbHolderRemediationTarget::from_holder(&holder_with_start(
            77,
            "mempal_mcp_server",
            "stale_mcp_server",
            Some(1_000),
        ));

        assert!(target.matches_holder(&holder_with_start(
            77,
            "mempal_mcp_server",
            "stale_mcp_server",
            Some(1_000),
        )));
        assert!(!target.matches_holder(&holder_with_start(
            77,
            "mempal_mcp_server",
            "stale_mcp_server",
            None,
        )));
        assert!(!target.matches_holder(&holder_with_start(
            77,
            "mempal_mcp_server",
            "stale_mcp_server",
            Some(2_000),
        )));
        assert!(!target.matches_holder(&holder_with_start(
            77,
            "other",
            "extra_holder",
            Some(1_000),
        )));
        assert!(!target.matches_holder(&holder_with_start(
            77,
            "mempal_mcp_server",
            "current_mcp_server",
            Some(1_000),
        )));
    }

    #[test]
    fn test_format_db_lock_remediation_hint_reports_roles_pids_and_hints() {
        let report = build_report(
            Path::new("/tmp/palace.db"),
            vec![
                holder(33, "other", "extra_holder"),
                holder(44, "mempal_daemon", "current_daemon"),
            ],
            None,
        );

        let message = format_db_lock_remediation_hint(
            Path::new("/tmp/palace.db"),
            "database is locked",
            &report,
            &[11, 22],
            &["SIGTERM pid 22 failed: permission denied".to_string()],
        );

        assert!(message.contains("failed to open daemon database /tmp/palace.db"));
        assert!(message.contains("automatically terminated stale mempal-owned DB holders: 11, 22"));
        assert!(message.contains("pid=33 role=other classification=extra_holder"));
        assert!(message.contains("pid=44 role=mempal_daemon classification=current_daemon"));
        assert!(message.contains("mempal only auto-terminates stale_mcp_server and orphan_daemon"));
        assert!(message.contains("mempal daemon status"));
    }

    #[test]
    fn test_db_holder_role_summary_uses_sanitized_fields() {
        let report = build_report(
            Path::new("/tmp/palace.db"),
            vec![holder(55, "mempal_mcp_server", "current_mcp_server")],
            None,
        );

        let summary = format_db_holder_role_summary(&report);

        assert!(summary.contains("pid=55"));
        assert!(summary.contains("role=mempal_mcp_server"));
        assert!(summary.contains("classification=current_mcp_server"));
        assert!(!summary.contains("content"));
        assert!(!summary.contains("token"));
    }

    #[test]
    fn test_sqlite_lock_safe_next_step_distinguishes_current_mcp() {
        let report = build_report(
            Path::new("/tmp/palace.db"),
            vec![holder(55, "mempal_mcp_server", "current_mcp_server")],
            None,
        );

        let step = sqlite_lock_safe_next_step(&report);

        assert!(step.contains("current MCP server"));
        assert!(step.contains("without killing the server"));
    }

    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::*;
        use crate::db_path_identity::{append_os_suffix, db_file_targets_with_cwd};
        use std::fs;
        use std::fs::File;
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;

        fn write_process(proc_root: &Path, pid: i32, argv: &[&str], db_files: &[&Path]) {
            write_process_with_parent(proc_root, pid, 0, argv, db_files);
        }

        fn write_process_with_parent(
            proc_root: &Path,
            pid: i32,
            parent_pid: i32,
            argv: &[&str],
            db_files: &[&Path],
        ) {
            let pid_dir = proc_root.join(pid.to_string());
            let fd_dir = pid_dir.join("fd");
            fs::create_dir_all(&fd_dir).expect("create fake fd dir");
            fs::write(pid_dir.join("cmdline"), argv.join("\0")).expect("write cmdline");
            fs::write(
                pid_dir.join("stat"),
                format!("{pid} (mempal) S {parent_pid} 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 250"),
            )
            .expect("write stat");
            for (index, file) in db_files.iter().enumerate() {
                symlink(file, fd_dir.join((index + 3).to_string())).expect("symlink fd");
            }
        }

        fn opened_db_files_for_test(
            fd_dir: &Path,
            targets: &[(&'static str, DbFileIdentity)],
        ) -> Vec<String> {
            let mut deadline = DbHolderInspectionDeadline::none();
            match opened_db_files(fd_dir, targets, &mut deadline) {
                OpenedDbFilesScan::Complete(opened) => opened,
                OpenedDbFilesScan::DeadlineExceeded => {
                    panic!("unbounded test scan unexpectedly timed out")
                }
            }
        }

        #[test]
        fn test_parse_cmdline_and_start_ticks() {
            assert_eq!(
                parse_cmdline(b"/usr/local/bin/mempal\0serve\0--mcp\0"),
                vec!["/usr/local/bin/mempal", "serve", "--mcp"]
            );
            assert!(is_mcp_server_argv(&[
                "/usr/local/bin/mempal".to_string(),
                "serve".to_string()
            ]));
            assert!(is_mcp_server_argv(&[
                "/usr/local/bin/mempal".to_string(),
                "--config-path".to_string(),
                "/tmp/config.toml".to_string(),
                "serve".to_string(),
                "--mcp".to_string()
            ]));
            assert!(is_mcp_server_argv(&[
                "/usr/local/bin/mempal".to_string(),
                "--config-path=/tmp/config.toml".to_string(),
                "serve".to_string(),
                "--mcp".to_string()
            ]));
            let stat = b"123 (mempal worker) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 98765";
            assert_eq!(parse_start_ticks(stat), Some(98765));
        }

        #[test]
        fn test_read_start_ticks_handles_non_utf8_process_name() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let stat_path = tmp.path().join("stat");
            let mut stat = b"123 (mempal ".to_vec();
            stat.extend_from_slice(&[0xff, 0xfe]);
            stat.extend_from_slice(b") S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 98765");
            fs::write(&stat_path, stat).expect("write stat");

            assert_eq!(read_start_ticks(&stat_path), Some(98765));
        }

        #[test]
        fn test_command_display_uses_fixed_labels_only() {
            assert_eq!(command_display("mempal_mcp_server"), "mempal serve");
            assert_eq!(command_display("mempal_daemon"), "mempal daemon");
            assert_eq!(command_display("mempal_cli"), "mempal cli");
            assert_eq!(command_display("other"), "other process");
        }

        #[test]
        fn test_daemon_pid_path_uses_current_dir_for_bare_relative_db_path() {
            assert_eq!(
                daemon_pid_path(Path::new("palace.db")),
                PathBuf::from(".").join("daemon.pid")
            );
            assert_eq!(
                daemon_pid_path(Path::new("data/palace.db")),
                PathBuf::from("data").join("daemon.pid")
            );
        }

        #[test]
        fn test_inspect_db_holders_classifies_stale_mcp_and_orphan_daemon() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::write(proc_root.join("stat"), "cpu 0 0 0 0\nbtime 1000\n").expect("write stat");

            let mempal_home = tmp.path().join(".mempal");
            fs::create_dir_all(&mempal_home).expect("create mempal home");
            let db_path = mempal_home.join("palace.db");
            let wal_path = append_os_suffix(&db_path, "-wal");
            let shm_path = append_os_suffix(&db_path, "-shm");
            fs::write(mempal_home.join("daemon.pid"), "42\n").expect("write pidfile");

            write_process(
                &proc_root,
                42,
                &["mempal", "daemon", "--foreground"],
                &[db_path.as_path()],
            );
            write_process_with_parent(
                &proc_root,
                77,
                1,
                &["/usr/local/bin/mempal", "serve"],
                &[db_path.as_path(), wal_path.as_path(), shm_path.as_path()],
            );
            write_process(
                &proc_root,
                88,
                &["mempal", "daemon", "--foreground"],
                &[db_path.as_path()],
            );
            write_process(
                &proc_root,
                99,
                &["sqlite3", "palace.db"],
                &[wal_path.as_path()],
            );

            let report = inspect_db_holders_in_proc(&db_path, &proc_root, 1100, 100);

            assert_eq!(report.holder_count, 4);
            assert_eq!(report.stale_mcp_server_count, 1);
            assert_eq!(report.orphan_daemon_count, 1);
            assert_eq!(report.extra_holder_count, 1);
            assert!(report.has_problem());
            assert_eq!(report.holders[0].classification, "current_daemon");
            assert_eq!(report.holders[1].classification, "stale_mcp_server");
            assert_eq!(report.holders[1].opened_files, vec!["db", "shm", "wal"]);
            assert_eq!(report.holders[1].started_at_unix_secs, Some(1002));
            assert_eq!(report.holders[1].age_secs, Some(98));
            assert_eq!(report.holders[2].classification, "orphan_daemon");
            assert_eq!(report.holders[3].classification, "extra_holder");
        }

        #[test]
        fn test_inspect_db_holders_deadline_returns_incomplete_report() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::write(proc_root.join("stat"), "cpu 0 0 0 0\nbtime 1000\n").expect("write stat");

            let mempal_home = tmp.path().join(".mempal");
            fs::create_dir_all(&mempal_home).expect("create mempal home");
            let db_path = mempal_home.join("palace.db");
            write_process(
                &proc_root,
                42,
                &["mempal", "daemon", "--foreground"],
                &[db_path.as_path()],
            );

            let report = inspect_db_holders_in_proc_with_deadline(
                &db_path,
                &proc_root,
                1100,
                100,
                Instant::now(),
            );

            assert_eq!(report.holder_count, 0);
            assert!(
                report
                    .error
                    .as_deref()
                    .is_some_and(|error| error.contains("time budget")),
                "{report:#?}"
            );
            assert!(report.has_problem());
        }

        #[test]
        fn test_inspect_db_holders_deadline_expires_during_fd_scan() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::write(proc_root.join("stat"), "cpu 0 0 0 0\nbtime 1000\n").expect("write stat");

            let mempal_home = tmp.path().join(".mempal");
            fs::create_dir_all(&mempal_home).expect("create mempal home");
            let db_path = mempal_home.join("palace.db");
            let other_path = mempal_home.join("other.db");
            write_process(
                &proc_root,
                42,
                &["mempal", "daemon", "--foreground"],
                &[other_path.as_path(), db_path.as_path()],
            );

            let report = inspect_db_holders_in_proc_with_daemon_pid_protection(
                &db_path,
                &proc_root,
                1100,
                100,
                true,
                DbHolderInspectionDeadline::after_fd_entries(1),
            );

            assert_eq!(report.holder_count, 0);
            assert_eq!(report.error.as_deref(), Some(DB_HOLDER_INSPECTION_TIMEOUT));
            assert!(report.has_problem());
        }

        #[test]
        fn test_startup_remediation_treats_stale_daemon_pidfile_as_orphan() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::write(proc_root.join("stat"), "cpu 0 0 0 0\nbtime 1000\n").expect("write stat");

            let mempal_home = tmp.path().join(".mempal");
            fs::create_dir_all(&mempal_home).expect("create mempal home");
            let db_path = mempal_home.join("palace.db");
            fs::write(mempal_home.join("daemon.pid"), "42042\n").expect("write pidfile");

            write_process(
                &proc_root,
                42042,
                &["mempal", "daemon", "--foreground"],
                &[db_path.as_path()],
            );
            write_process_with_parent(
                &proc_root,
                7777,
                1234,
                &["/usr/local/bin/mempal", "serve"],
                &[db_path.as_path()],
            );

            let report =
                inspect_db_holders_in_proc_for_startup_remediation(&db_path, &proc_root, 1100, 100);

            assert_eq!(report.holder_count, 2);
            assert_eq!(report.orphan_daemon_count, 1);
            assert_eq!(report.stale_mcp_server_count, 0);
            assert_eq!(report.extra_holder_count, 1);
            let daemon_holder = report
                .holders
                .iter()
                .find(|holder| holder.pid == 42042)
                .expect("daemon holder");
            assert_eq!(daemon_holder.role, "mempal_daemon");
            assert_eq!(daemon_holder.classification, "orphan_daemon");
            assert!(!daemon_holder.current_daemon);

            let mcp_holder = report
                .holders
                .iter()
                .find(|holder| holder.pid == 7777)
                .expect("mcp holder");
            assert_eq!(mcp_holder.role, "mempal_mcp_server");
            assert_eq!(mcp_holder.classification, "extra_holder");

            let plan = plan_stale_db_holder_remediation(&report);
            assert_eq!(plan.terminate_pids, vec![42042]);
            assert!(plan.terminate_targets[0].matches_holder(daemon_holder));
            assert_eq!(
                plan.manual_holders
                    .iter()
                    .map(|holder| holder.pid)
                    .collect::<Vec<_>>(),
                vec![7777]
            );
        }

        #[test]
        fn test_inspect_db_holders_keeps_supervised_mcp_manual() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::write(proc_root.join("stat"), "cpu 0 0 0 0\nbtime 1000\n").expect("write stat");

            let db_path = tmp.path().join("palace.db");
            write_process_with_parent(
                &proc_root,
                77,
                1234,
                &["/usr/local/bin/mempal", "serve"],
                &[db_path.as_path()],
            );

            let report = inspect_db_holders_in_proc(&db_path, &proc_root, 1100, 100);

            assert_eq!(report.holder_count, 1);
            assert_eq!(report.stale_mcp_server_count, 0);
            assert_eq!(report.extra_holder_count, 1);
            assert_eq!(report.holders[0].role, "mempal_mcp_server");
            assert_eq!(report.holders[0].classification, "extra_holder");
            let plan = plan_stale_db_holder_remediation(&report);
            assert!(plan.terminate_pids.is_empty());
            assert_eq!(
                plan.manual_holders
                    .iter()
                    .map(|holder| holder.pid)
                    .collect::<Vec<_>>(),
                vec![77]
            );
        }

        #[test]
        fn test_inspect_db_holders_ignores_non_db_fds_and_quoted_commands() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::write(proc_root.join("stat"), "btime 1000\n").expect("write proc stat");

            let db_path = tmp.path().join("palace.db");
            let other_path = tmp.path().join("other.db");
            write_process(
                &proc_root,
                11,
                &["claude", "-p", "mempal serve --mcp"],
                &[db_path.as_path()],
            );
            write_process(
                &proc_root,
                22,
                &["mempal", "serve", "--mcp"],
                &[other_path.as_path()],
            );

            let report = inspect_db_holders_in_proc(&db_path, &proc_root, 1100, 100);

            assert_eq!(report.holder_count, 1);
            assert_eq!(report.holders[0].role, "other");
            assert_eq!(report.holders[0].classification, "extra_holder");
        }

        #[test]
        fn test_inspect_db_holders_redacts_sensitive_argv_from_diagnostics() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::write(proc_root.join("stat"), "btime 1000\n").expect("write proc stat");

            let db_path = tmp.path().join("palace.db");
            let private_config = tmp.path().join("private").join("config.toml");
            write_process(
                &proc_root,
                77,
                &[
                    "/private/bin/mempal",
                    "serve",
                    "--mcp",
                    "--api-key",
                    "sk-test-secret",
                    "--config-path",
                    private_config.to_str().expect("utf8 path"),
                    "--token=ghp_test_secret",
                ],
                &[db_path.as_path()],
            );
            write_process(
                &proc_root,
                99,
                &[
                    "/private/tools/sqlite3",
                    db_path.to_str().expect("utf8 path"),
                    "--password=hunter2",
                    "OPENAI_API_KEY=sk-hidden",
                ],
                &[db_path.as_path()],
            );

            let report = inspect_db_holders_in_proc(&db_path, &proc_root, 1100, 100);
            let serialized = serde_json::to_string(&report).expect("serialize report");

            assert_eq!(report.holder_count, 2);
            assert_eq!(report.holders[0].command, "mempal serve");
            assert_eq!(report.holders[1].command, "other process");
            for forbidden in [
                "--api-key",
                "sk-test-secret",
                "--config-path",
                private_config.to_str().expect("utf8 path"),
                "--token=ghp_test_secret",
                "--password=hunter2",
                "OPENAI_API_KEY=sk-hidden",
                "/private/bin/mempal",
                "/private/tools/sqlite3",
            ] {
                assert!(
                    !serialized.contains(forbidden),
                    "db holder diagnostics leaked raw argv fragment: {forbidden}"
                );
            }
        }

        #[test]
        fn test_opened_db_files_matches_relative_db_path_against_absolute_fd_target() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            fs::create_dir_all(&proc_root).expect("create proc");
            let db_path = tmp.path().join("palace.db");
            File::create(&db_path).expect("db file");
            write_process(&proc_root, 33, &["mempal", "serve"], &[db_path.as_path()]);

            let targets = db_file_targets_with_cwd(Path::new("palace.db"), tmp.path());
            let opened = opened_db_files_for_test(&proc_root.join("33").join("fd"), &targets);

            assert_eq!(opened, vec!["db"]);
        }

        #[test]
        fn test_opened_db_files_matches_symlinked_db_directory() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            let real_home = tmp.path().join("real-mempal");
            let linked_home = tmp.path().join("linked-mempal");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::create_dir_all(&real_home).expect("create real home");
            symlink(&real_home, &linked_home).expect("symlink home");
            let real_db_path = real_home.join("palace.db");
            File::create(&real_db_path).expect("db file");
            write_process(
                &proc_root,
                44,
                &["mempal", "serve"],
                &[real_db_path.as_path()],
            );

            let targets = db_file_targets(&linked_home.join("palace.db"));
            let opened = opened_db_files_for_test(&proc_root.join("44").join("fd"), &targets);

            assert_eq!(opened, vec!["db"]);
        }

        #[test]
        fn test_opened_db_files_matches_symlinked_db_path() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path().join("proc");
            let real_home = tmp.path().join("real-mempal");
            fs::create_dir_all(&proc_root).expect("create proc");
            fs::create_dir_all(&real_home).expect("create real home");
            let real_db_path = real_home.join("palace.db");
            let linked_db_path = tmp.path().join("palace-link.db");
            File::create(&real_db_path).expect("db file");
            symlink(&real_db_path, &linked_db_path).expect("symlink db");
            write_process(
                &proc_root,
                55,
                &["mempal", "serve"],
                &[real_db_path.as_path()],
            );

            let targets = db_file_targets(&linked_db_path);
            let opened = opened_db_files_for_test(&proc_root.join("55").join("fd"), &targets);

            assert_eq!(opened, vec!["db"]);
        }

        #[test]
        fn test_inspect_process_memory_reports_rss_pss_private_dirty_and_deleted_exe() {
            let tmp = tempfile::tempdir().expect("tempdir");
            let proc_root = tmp.path();
            let pid_dir = proc_root.join("321");
            fs::create_dir_all(&pid_dir).expect("create pid dir");
            fs::write(
                pid_dir.join("status"),
                "Name:\tmempal\nVmRSS:\t  2048 kB\nVmHWM:\t  4096 kB\n",
            )
            .expect("write status");
            fs::write(
                pid_dir.join("smaps_rollup"),
                "Rss: 2048 kB\nPss: 1536 kB\nPrivate_Dirty: 1024 kB\nAnonymous: 768 kB\nSwap: 256 kB\n",
            )
            .expect("write smaps");
            fs::write(
                pid_dir.join("io"),
                "rchar: 100\nwchar: 200\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 1024\n",
            )
            .expect("write io");
            symlink("/usr/local/bin/mempal (deleted)", pid_dir.join("exe")).expect("symlink exe");

            let report = inspect_process_memory_in_proc(321, proc_root);

            assert_eq!(report.rss_bytes, Some(2_097_152));
            assert_eq!(report.pss_bytes, Some(1_572_864));
            assert_eq!(report.vm_hwm_bytes, Some(4_194_304));
            assert_eq!(report.private_dirty_bytes, Some(1_048_576));
            assert_eq!(report.anonymous_bytes, Some(786_432));
            assert_eq!(report.swap_bytes, Some(262_144));
            assert_eq!(report.io_read_bytes, Some(4_096));
            assert_eq!(report.io_write_bytes, Some(8_192));
            assert_eq!(report.io_cancelled_write_bytes, Some(1_024));
            assert!(report.exe_deleted);
            assert_eq!(
                report.exe_path.as_deref(),
                Some("/usr/local/bin/mempal (deleted)")
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    mod non_linux {
        use super::super::*;
        use std::path::Path;

        #[test]
        fn test_inspect_db_holders_noops_without_error() {
            let report = inspect_db_holders(Path::new("/tmp/palace.db"));

            assert_eq!(report.holder_count, 0);
            assert_eq!(report.extra_holder_count, 0);
            assert_eq!(report.stale_mcp_server_count, 0);
            assert_eq!(report.orphan_daemon_count, 0);
            assert!(report.error.is_none());
            assert!(!report.has_problem());
        }
    }
}
