//! REST resource status assembled from headless diagnostics.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::core::async_db::AsyncDbResourceSnapshot;
use crate::core::db_admission::ProfileDbAdmission;

#[derive(Debug, Default, Serialize)]
pub(super) struct ResourceUsageStatus {
    process: ProcessResourceUsageStatus,
    sqlite: SqliteResourceUsageStatus,
    profile_admission: ProfileAdmissionStatus,
    memory_pressure: crate::system_memory::MemoryPressureSnapshot,
    daemon_recovery: DaemonRecoveryStatus,
    counters: ResourceCounterStatus,
}

pub(super) fn build_resource_usage(
    db_path: &Path,
    sqlite: Option<AsyncDbResourceSnapshot>,
) -> ResourceUsageStatus {
    let process = crate::process_diagnostics::inspect_process_memory(std::process::id() as i32);
    ResourceUsageStatus {
        process: ProcessResourceUsageStatus::from(process),
        sqlite: sqlite
            .map(SqliteResourceUsageStatus::from)
            .unwrap_or_default(),
        profile_admission: ProfileAdmissionStatus::inspect(db_path),
        memory_pressure: crate::system_memory::inspect_memory_pressure(),
        daemon_recovery: DaemonRecoveryStatus::inspect(db_path.parent().unwrap_or(db_path)),
        counters: ResourceCounterStatus::from(crate::observability::resource_counters()),
    }
}

/// Degraded status used when spawn_blocking panics or is cancelled.
pub(super) fn build_resource_usage_degraded() -> ResourceUsageStatus {
    ResourceUsageStatus {
        process: ProcessResourceUsageStatus::default(),
        sqlite: SqliteResourceUsageStatus::default(),
        profile_admission: ProfileAdmissionStatus::unavailable(),
        memory_pressure: crate::system_memory::MemoryPressureSnapshot::default(),
        daemon_recovery: DaemonRecoveryStatus::default(),
        counters: ResourceCounterStatus::from(crate::observability::resource_counters()),
    }
}

#[derive(Debug, Default, Serialize)]
struct DaemonRecoveryStatus {
    phase: String,
    recent_fault_count: usize,
    restart_budget_remaining: usize,
    cooldown_remaining_secs: u64,
    last_fault: Option<String>,
    error: Option<String>,
}

impl DaemonRecoveryStatus {
    fn inspect(mempal_home: &Path) -> Self {
        match crate::daemon_recovery::DaemonRecovery::new(mempal_home).snapshot() {
            Ok(snapshot) => Self {
                phase: snapshot.phase.as_str().to_string(),
                recent_fault_count: snapshot.recent_fault_count,
                restart_budget_remaining: snapshot.restart_budget_remaining,
                cooldown_remaining_secs: snapshot.cooldown_remaining_secs,
                last_fault: snapshot.last_fault.map(|fault| fault.as_str().to_string()),
                error: None,
            },
            Err(error) => Self {
                phase: "unavailable".to_string(),
                error: Some(error.to_string()),
                ..Self::default()
            },
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct ProcessResourceUsageStatus {
    pid: i32,
    rss_bytes: Option<u64>,
    pss_bytes: Option<u64>,
    private_dirty_bytes: Option<u64>,
    anonymous_bytes: Option<u64>,
    swap_bytes: Option<u64>,
    io_read_bytes: Option<u64>,
    io_write_bytes: Option<u64>,
    io_cancelled_write_bytes: Option<u64>,
}

impl From<crate::process_diagnostics::ProcessMemoryReport> for ProcessResourceUsageStatus {
    fn from(value: crate::process_diagnostics::ProcessMemoryReport) -> Self {
        Self {
            pid: value.pid,
            rss_bytes: value.rss_bytes,
            pss_bytes: value.pss_bytes,
            private_dirty_bytes: value.private_dirty_bytes,
            anonymous_bytes: value.anonymous_bytes,
            swap_bytes: value.swap_bytes,
            io_read_bytes: value.io_read_bytes,
            io_write_bytes: value.io_write_bytes,
            io_cancelled_write_bytes: value.io_cancelled_write_bytes,
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct SqliteResourceUsageStatus {
    async_pool_loaded: bool,
    async_reader_connections: usize,
    async_writer_connections: usize,
    async_total_connections: usize,
    per_connection_cache_kib: i64,
    per_connection_cache_bytes: u64,
    configured_page_cache_bytes: u64,
    page_cache_budget_bytes: u64,
}

impl From<AsyncDbResourceSnapshot> for SqliteResourceUsageStatus {
    fn from(value: AsyncDbResourceSnapshot) -> Self {
        Self {
            async_pool_loaded: true,
            async_reader_connections: value.reader_connections,
            async_writer_connections: value.writer_connections,
            async_total_connections: value.total_connections,
            per_connection_cache_kib: value.per_connection_cache_kib,
            per_connection_cache_bytes: value.per_connection_cache_bytes,
            configured_page_cache_bytes: value.configured_page_cache_bytes,
            page_cache_budget_bytes: value.page_cache_budget_bytes,
        }
    }
}

#[derive(Debug, Default, Serialize)]
struct ProfileAdmissionStatus {
    active_holders: usize,
    reaped_stale_holders_this_snapshot: usize,
    unknown_holders: usize,
    unknown_holder_generations: Vec<u64>,
    unknown_holder_diagnostics: Vec<UnknownHolderDiagnosticStatus>,
    configured_holder_limit: usize,
    reserved_service_holders: usize,
    service_holders: usize,
    configured_cache_bytes: u64,
    active_cache_bytes: u64,
    available_cache_bytes: u64,
    holders: Vec<ProfileAdmissionHolderStatus>,
    error: Option<String>,
}

impl ProfileAdmissionStatus {
    fn unavailable() -> Self {
        Self {
            error: Some("profile admission diagnostics unavailable".to_string()),
            ..Self::default()
        }
    }

    fn inspect(db_path: &Path) -> Self {
        let snapshot = match ProfileDbAdmission::snapshot(db_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return Self {
                    error: Some(error.to_string()),
                    ..Self::default()
                };
            }
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Self {
            active_holders: snapshot.active_holders,
            reaped_stale_holders_this_snapshot: snapshot.reaped_stale_holders_this_snapshot,
            unknown_holders: snapshot.unknown_holders,
            unknown_holder_generations: snapshot.unknown_holder_generations,
            unknown_holder_diagnostics: snapshot
                .unknown_holder_diagnostics
                .into_iter()
                .map(|diagnostic| UnknownHolderDiagnosticStatus {
                    generation: diagnostic.generation,
                    reason: diagnostic.reason.to_string(),
                })
                .collect(),
            configured_holder_limit: snapshot.configured_holder_limit,
            reserved_service_holders: snapshot.reserved_service_holders,
            service_holders: snapshot.service_holders,
            configured_cache_bytes: snapshot.configured_cache_bytes,
            active_cache_bytes: snapshot.active_cache_bytes,
            available_cache_bytes: snapshot.available_cache_bytes,
            holders: snapshot
                .holders
                .into_iter()
                .map(|holder| ProfileAdmissionHolderStatus {
                    holder_class: holder.holder_class.to_string(),
                    owner_identity: holder.owner_identity,
                    pid: holder.pid,
                    generation: holder.generation,
                    age_secs: now.saturating_sub(holder.acquired_at_unix_secs),
                    connection_count: holder.connection_count,
                    configured_cache_bytes: holder.configured_cache_bytes,
                })
                .collect(),
            error: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct UnknownHolderDiagnosticStatus {
    generation: u64,
    reason: String,
}

#[derive(Debug, Serialize)]
struct ProfileAdmissionHolderStatus {
    holder_class: String,
    owner_identity: String,
    pid: u32,
    generation: u64,
    age_secs: u64,
    connection_count: usize,
    configured_cache_bytes: u64,
}

#[derive(Debug, Default, Serialize)]
struct ResourceCounterStatus {
    access_writeback_scheduled_total: u64,
    access_writeback_skipped_total: u64,
    access_writeback_failed_total: u64,
}

impl From<crate::observability::ResourceCounterSnapshot> for ResourceCounterStatus {
    fn from(value: crate::observability::ResourceCounterSnapshot) -> Self {
        Self {
            access_writeback_scheduled_total: value.access_writeback_scheduled_total,
            access_writeback_skipped_total: value.access_writeback_skipped_total,
            access_writeback_failed_total: value.access_writeback_failed_total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db_admission::{DbAdmissionRequest, DbHolderClass};

    #[test]
    fn status_serializes_profile_admission_memory_pressure_and_recovery() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        let _holder = ProfileDbAdmission::acquire(
            &db_path,
            DbAdmissionRequest::new(DbHolderClass::Api, 2, 8 * 1024 * 1024),
        )
        .expect("admit API holder");

        let value = serde_json::to_value(build_resource_usage(&db_path, None))
            .expect("serialize resource status");

        assert_eq!(value["profile_admission"]["active_holders"], 1);
        assert_eq!(
            value["profile_admission"]["holders"][0]["holder_class"],
            "api"
        );
        assert!(value["memory_pressure"].is_object());
        assert_eq!(value["daemon_recovery"]["phase"], "healthy");
    }

    #[test]
    fn degraded_resource_usage_marks_admission_diagnostics_unavailable() {
        let status = build_resource_usage_degraded();

        assert!(
            status.profile_admission.error.is_some(),
            "degraded status must not serialize admission diagnostics as healthy zeroes"
        );
    }
}
