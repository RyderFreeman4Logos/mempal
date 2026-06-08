#[cfg(target_os = "linux")]
use std::fs;
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};

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

/// One live process with an open fd to `palace.db`, `palace.db-wal`, or
/// `palace.db-shm`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct DbHolderProcess {
    pub pid: i32,
    pub role: String,
    pub classification: String,
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

#[cfg(target_os = "linux")]
fn inspect_db_holders_in_proc(
    db_path: &Path,
    proc_root: &Path,
    now_secs: u64,
    clock_ticks_per_second: u64,
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
        let Some(pid) = parse_pid(entry.file_name().to_string_lossy().as_ref()) else {
            continue;
        };
        let pid_dir = entry.path();
        let opened_files = opened_db_files(&pid_dir.join("fd"), &targets);
        if opened_files.is_empty() {
            continue;
        }

        let argv = read_cmdline(&pid_dir.join("cmdline"));
        let role = classify_role(&argv, &binary_name);
        let current_process = pid == current_pid;
        let current_daemon = daemon_pid == Some(pid) && role == "mempal_daemon";
        let current_mcp_server = current_process && role == "mempal_mcp_server";
        let classification =
            classify_holder(role, current_process, current_daemon, current_mcp_server);
        let started_at_unix_secs = boot_time.and_then(|btime| {
            read_start_ticks(&pid_dir.join("stat")).map(|ticks| {
                btime.saturating_add(ticks.saturating_div(clock_ticks_per_second.max(1)))
            })
        });
        let age_secs = started_at_unix_secs.map(|started| now_secs.saturating_sub(started));

        holders.push(DbHolderProcess {
            pid,
            role: role.to_string(),
            classification: classification.to_string(),
            command: command_display(&argv),
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
        .filter(|holder| {
            !matches!(
                holder.classification.as_str(),
                "current_process" | "current_daemon" | "current_mcp_server"
            )
        })
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
fn opened_db_files(fd_dir: &Path, targets: &[(String, PathBuf)]) -> Vec<String> {
    let entries = match fs::read_dir(fd_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut opened = Vec::new();
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let target = strip_deleted_suffix(target);
        for (kind, expected) in targets {
            if target == *expected && !opened.contains(kind) {
                opened.push(kind.clone());
            }
        }
    }
    opened.sort();
    opened
}

#[cfg(target_os = "linux")]
fn db_file_targets(db_path: &Path) -> Vec<(String, PathBuf)> {
    vec![
        ("db".to_string(), db_path.to_path_buf()),
        ("shm".to_string(), path_with_suffix(db_path, "-shm")),
        ("wal".to_string(), path_with_suffix(db_path, "-wal")),
    ]
}

#[cfg(target_os = "linux")]
fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

#[cfg(target_os = "linux")]
fn strip_deleted_suffix(path: PathBuf) -> PathBuf {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    const DELETED_SUFFIX: &[u8] = b" (deleted)";
    let bytes = path.as_os_str().as_bytes();
    if bytes.ends_with(DELETED_SUFFIX) {
        let keep = bytes.len().saturating_sub(DELETED_SUFFIX.len());
        return PathBuf::from(OsString::from_vec(bytes[..keep].to_vec()));
    }
    path
}

#[cfg(target_os = "linux")]
fn read_daemon_pid(db_path: &Path) -> Option<i32> {
    let pid_path = db_path.parent()?.join("daemon.pid");
    fs::read_to_string(pid_path)
        .ok()
        .and_then(|content| content.trim().parse::<i32>().ok())
}

#[cfg(target_os = "linux")]
fn read_cmdline(path: &Path) -> Vec<String> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    parse_cmdline(&bytes)
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
    "mempal_cli"
}

#[cfg(target_os = "linux")]
fn classify_holder(
    role: &str,
    current_process: bool,
    current_daemon: bool,
    current_mcp_server: bool,
) -> &'static str {
    if current_daemon {
        "current_daemon"
    } else if current_mcp_server {
        "current_mcp_server"
    } else if current_process {
        "current_process"
    } else if role == "mempal_mcp_server" {
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
    let Some((_, subcommand)) = first_cli_subcommand(argv) else {
        return false;
    };
    subcommand == "serve"
}

#[cfg(target_os = "linux")]
fn first_cli_subcommand(argv: &[String]) -> Option<(usize, &str)> {
    let mut index = 1;
    while index < argv.len() {
        let arg = argv[index].as_str();
        if arg == "--" {
            return argv.get(index + 1).map(|value| (index + 1, value.as_str()));
        }
        if global_flag_takes_value(arg) {
            index += 2;
            continue;
        }
        if global_flag_has_inline_value(arg) || arg.starts_with('-') {
            index += 1;
            continue;
        }
        return Some((index, arg));
    }
    None
}

#[cfg(target_os = "linux")]
fn global_flag_takes_value(arg: &str) -> bool {
    matches!(arg, "--config" | "--db-path")
}

#[cfg(target_os = "linux")]
fn global_flag_has_inline_value(arg: &str) -> bool {
    arg.starts_with("--config=") || arg.starts_with("--db-path=")
}

#[cfg(target_os = "linux")]
fn command_display(argv: &[String]) -> String {
    if argv.is_empty() {
        return "<unknown>".to_string();
    }
    argv.join(" ")
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
    let content = fs::read_to_string(path).ok()?;
    parse_start_ticks(&content)
}

#[cfg(target_os = "linux")]
fn parse_start_ticks(stat: &str) -> Option<u64> {
    let close = stat.rfind(") ")?;
    let fields = stat[close + 2..].split_whitespace().collect::<Vec<_>>();
    fields.get(19)?.parse::<u64>().ok()
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
    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::*;
        use std::fs;
        use std::os::unix::fs::symlink;

        fn write_process(proc_root: &Path, pid: i32, argv: &[&str], db_files: &[&Path]) {
            let pid_dir = proc_root.join(pid.to_string());
            let fd_dir = pid_dir.join("fd");
            fs::create_dir_all(&fd_dir).expect("create fake fd dir");
            fs::write(pid_dir.join("cmdline"), argv.join("\0")).expect("write cmdline");
            fs::write(
                pid_dir.join("stat"),
                format!("{pid} (mempal) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 250"),
            )
            .expect("write stat");
            for (index, file) in db_files.iter().enumerate() {
                symlink(file, fd_dir.join((index + 3).to_string())).expect("symlink fd");
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
                "--db-path".to_string(),
                "/tmp/palace.db".to_string(),
                "serve".to_string(),
                "--mcp".to_string()
            ]));
            let stat = "123 (mempal worker) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 98765";
            assert_eq!(parse_start_ticks(stat), Some(98765));
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
            let wal_path = path_with_suffix(&db_path, "-wal");
            let shm_path = path_with_suffix(&db_path, "-shm");
            fs::write(mempal_home.join("daemon.pid"), "42\n").expect("write pidfile");

            write_process(
                &proc_root,
                42,
                &["mempal", "daemon", "--foreground"],
                &[db_path.as_path()],
            );
            write_process(
                &proc_root,
                77,
                &["/usr/local/bin/mempal", "serve"],
                &[db_path.as_path(), wal_path.as_path(), shm_path.as_path()],
            );
            write_process(
                &proc_root,
                88,
                &["mempal", "daemon", "--foreground"],
                &[db_path.as_path()],
            );

            let report = inspect_db_holders_in_proc(&db_path, &proc_root, 1100, 100);

            assert_eq!(report.holder_count, 3);
            assert_eq!(report.stale_mcp_server_count, 1);
            assert_eq!(report.orphan_daemon_count, 1);
            assert_eq!(report.extra_holder_count, 2);
            assert!(report.has_problem());
            assert_eq!(report.holders[0].classification, "current_daemon");
            assert_eq!(report.holders[1].classification, "stale_mcp_server");
            assert_eq!(report.holders[1].opened_files, vec!["db", "shm", "wal"]);
            assert_eq!(report.holders[1].started_at_unix_secs, Some(1002));
            assert_eq!(report.holders[1].age_secs, Some(98));
            assert_eq!(report.holders[2].classification, "orphan_daemon");
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
