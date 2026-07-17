//! CLI rendering for privacy-safe database and memory resource diagnostics.

use std::path::Path;

use crate::core::db_admission::ProfileDbAdmission;
use crate::process_diagnostics::DbHolderReport;

pub fn print_db_holder_report(heading: &str, report: &DbHolderReport, indent: &str) {
    println!("{heading}:");
    println!("{indent}path: {}", report.db_path);
    println!("{indent}total: {}", report.holder_count);
    println!("{indent}extra_holders: {}", report.extra_holder_count);
    println!(
        "{indent}stale_mcp_servers: {}",
        report.stale_mcp_server_count
    );
    println!("{indent}orphan_daemons: {}", report.orphan_daemon_count);
    if let Some(error) = report.error.as_deref() {
        println!("{indent}error: {error}");
    }
    if report.holders.is_empty() {
        println!("{indent}holders: none");
        return;
    }
    println!("{indent}holders:");
    for holder in &report.holders {
        let age = holder
            .age_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let started = holder
            .started_at_unix_secs
            .map(|value| value.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let files = if holder.opened_files.is_empty() {
            "none".to_string()
        } else {
            holder.opened_files.join(",")
        };
        println!(
            "{indent}- pid={} role={} classification={} current_process={} current_daemon={} current_mcp_server={} age_secs={} started_at_unix_secs={} files={} command={}",
            holder.pid,
            holder.role,
            holder.classification,
            holder.current_process,
            holder.current_daemon,
            holder.current_mcp_server,
            age,
            started,
            files,
            holder.command
        );
    }
}

pub fn print_profile_resource_status(db_path: &Path, indent: &str) {
    println!("Profile DB Admission:");
    match ProfileDbAdmission::snapshot(db_path) {
        Ok(snapshot) => {
            println!(
                "{indent}holders: {}/{}",
                snapshot.active_holders, snapshot.configured_holder_limit
            );
            println!(
                "{indent}cache_bytes: {}/{} available={}",
                snapshot.active_cache_bytes,
                snapshot.configured_cache_bytes,
                snapshot.available_cache_bytes
            );
            println!(
                "{indent}reaped_stale_holders: {}",
                snapshot.reaped_stale_holders
            );
            println!("{indent}unknown_holders: {}", snapshot.unknown_holders);
            if snapshot.unknown_holder_generations.is_empty() {
                println!("{indent}unknown_holder_generations: none");
            } else {
                println!(
                    "{indent}unknown_holder_generations: {}",
                    snapshot
                        .unknown_holder_generations
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                );
            }
            for holder in snapshot.holders {
                println!(
                    "{indent}- class={} owner={} pid={} generation={} connections={} cache_bytes={}",
                    holder.holder_class,
                    holder.owner_identity,
                    holder.pid,
                    holder.generation,
                    holder.connection_count,
                    holder.configured_cache_bytes
                );
            }
        }
        Err(error) => println!("{indent}error: {error}"),
    }

    let memory = crate::system_memory::inspect_memory_pressure();
    println!("Memory Pressure:");
    println!(
        "{indent}available_memory_bytes: {}",
        optional_u64(memory.available_memory_bytes)
    );
    println!(
        "{indent}cgroup_current_bytes: {}",
        optional_u64(memory.cgroup_current_bytes)
    );
    println!(
        "{indent}cgroup_limit_bytes: {}",
        optional_u64(memory.cgroup_limit_bytes)
    );
    println!(
        "{indent}cgroup_usage_percent: {}",
        optional_u64(memory.cgroup_usage_percent)
    );
    println!("{indent}pressure_high: {}", memory.pressure_high);
    if let Some(error) = memory.error {
        println!("{indent}error: {error}");
    }

    println!("Daemon Recovery:");
    match crate::daemon_recovery::DaemonRecovery::new(db_path.parent().unwrap_or(db_path))
        .snapshot()
    {
        Ok(snapshot) => {
            println!("{indent}phase: {}", snapshot.phase.as_str());
            println!(
                "{indent}recent_fault_count: {}",
                snapshot.recent_fault_count
            );
            println!(
                "{indent}restart_budget_remaining: {}",
                snapshot.restart_budget_remaining
            );
            println!(
                "{indent}cooldown_remaining_secs: {}",
                snapshot.cooldown_remaining_secs
            );
            println!(
                "{indent}last_fault: {}",
                snapshot.last_fault.map_or("none", |fault| fault.as_str())
            );
        }
        Err(error) => println!("{indent}error: {error}"),
    }
}

fn optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unavailable".to_string())
}
