use std::path::Path;
#[cfg(target_os = "linux")]
use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use crate::db_path_identity::{DbFileIdentity, db_file_targets};
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
        let file_name = entry.file_name();
        let Some(pid) = file_name.to_str().and_then(parse_pid) else {
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
fn opened_db_files(fd_dir: &Path, targets: &[(&'static str, DbFileIdentity)]) -> Vec<String> {
    let entries = match fs::read_dir(fd_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut opened = Vec::new();
    for entry in entries.flatten() {
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
    opened
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
    #[cfg(target_os = "linux")]
    mod linux {
        use super::super::*;
        use crate::db_path_identity::{append_os_suffix, db_file_targets_with_cwd};
        use std::fs;
        use std::fs::File;
        use std::os::unix::fs::symlink;
        use std::path::PathBuf;

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
            let opened = opened_db_files(&proc_root.join("33").join("fd"), &targets);

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
            let opened = opened_db_files(&proc_root.join("44").join("fd"), &targets);

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
            let opened = opened_db_files(&proc_root.join("55").join("fd"), &targets);

            assert_eq!(opened, vec!["db"]);
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
