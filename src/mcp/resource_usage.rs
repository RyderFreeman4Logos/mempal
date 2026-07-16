use rmcp::schemars::{self, JsonSchema};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub(crate) fn build_resource_usage(
    db_path: &Path,
    sqlite_snapshot: Option<crate::core::async_db::AsyncDbResourceSnapshot>,
) -> ResourceUsageDto {
    let sqlite = sqlite_snapshot
        .map(SqliteResourceUsageDto::from)
        .unwrap_or_else(|| SqliteResourceUsageDto {
            async_pool_loaded: false,
            ..SqliteResourceUsageDto::default()
        });
    let profile_admission = match crate::core::db_admission::ProfileDbAdmission::snapshot(db_path) {
        Ok(snapshot) => ProfileDbAdmissionDto::from(snapshot),
        Err(error) => ProfileDbAdmissionDto {
            error: Some(error.to_string()),
            ..ProfileDbAdmissionDto::default()
        },
    };
    ResourceUsageDto {
        process: ProcessResourceUsageDto::from(crate::process_diagnostics::inspect_process_memory(
            std::process::id() as i32,
        )),
        sqlite,
        profile_admission,
        memory_pressure: MemoryPressureDto::from(crate::system_memory::inspect_memory_pressure()),
        daemon_recovery: DaemonRecoveryDto::inspect(db_path.parent().unwrap_or(db_path)),
        counters: ResourceCounterDto::from(crate::observability::resource_counters()),
    }
}

/// Degraded status used when spawn_blocking panics or is cancelled.
pub(crate) fn build_resource_usage_degraded() -> ResourceUsageDto {
    ResourceUsageDto {
        counters: ResourceCounterDto::from(crate::observability::resource_counters()),
        ..ResourceUsageDto::default()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ResourceUsageDto {
    pub process: ProcessResourceUsageDto,
    pub sqlite: SqliteResourceUsageDto,
    pub profile_admission: ProfileDbAdmissionDto,
    pub memory_pressure: MemoryPressureDto,
    pub daemon_recovery: DaemonRecoveryDto,
    pub counters: ResourceCounterDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct DaemonRecoveryDto {
    pub phase: String,
    pub recent_fault_count: usize,
    pub restart_budget_remaining: usize,
    pub cooldown_remaining_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fault: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DaemonRecoveryDto {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileDbAdmissionDto {
    pub active_holders: usize,
    pub configured_holder_limit: usize,
    pub configured_cache_bytes: u64,
    pub active_cache_bytes: u64,
    pub available_cache_bytes: u64,
    pub holders: Vec<ProfileDbHolderDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProfileDbHolderDto {
    pub holder_class: String,
    pub owner_identity: String,
    pub pid: u32,
    pub generation: u64,
    pub age_secs: u64,
    pub connection_count: usize,
    pub configured_cache_bytes: u64,
}

impl From<crate::core::db_admission::DbAdmissionSnapshot> for ProfileDbAdmissionDto {
    fn from(value: crate::core::db_admission::DbAdmissionSnapshot) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        Self {
            active_holders: value.active_holders,
            configured_holder_limit: value.configured_holder_limit,
            configured_cache_bytes: value.configured_cache_bytes,
            active_cache_bytes: value.active_cache_bytes,
            available_cache_bytes: value.available_cache_bytes,
            holders: value
                .holders
                .into_iter()
                .map(|holder| ProfileDbHolderDto {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct MemoryPressureDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_memory_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroup_current_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroup_limit_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cgroup_usage_percent: Option<u64>,
    pub pressure_high: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl From<crate::system_memory::MemoryPressureSnapshot> for MemoryPressureDto {
    fn from(value: crate::system_memory::MemoryPressureSnapshot) -> Self {
        Self {
            available_memory_bytes: value.available_memory_bytes,
            cgroup_current_bytes: value.cgroup_current_bytes,
            cgroup_limit_bytes: value.cgroup_limit_bytes,
            cgroup_usage_percent: value.cgroup_usage_percent,
            pressure_high: value.pressure_high,
            error: value.error,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ProcessResourceUsageDto {
    pub pid: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rss_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pss_bytes: Option<u64>,
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
}

impl From<crate::process_diagnostics::ProcessMemoryReport> for ProcessResourceUsageDto {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SqliteResourceUsageDto {
    pub async_pool_loaded: bool,
    pub async_reader_connections: usize,
    pub async_writer_connections: usize,
    pub async_total_connections: usize,
    pub per_connection_cache_kib: i64,
    pub per_connection_cache_bytes: u64,
    pub configured_page_cache_bytes: u64,
    pub page_cache_budget_bytes: u64,
}

impl From<crate::core::async_db::AsyncDbResourceSnapshot> for SqliteResourceUsageDto {
    fn from(value: crate::core::async_db::AsyncDbResourceSnapshot) -> Self {
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ResourceCounterDto {
    pub access_writeback_scheduled_total: u64,
    pub access_writeback_skipped_total: u64,
    pub access_writeback_failed_total: u64,
}

impl From<crate::observability::ResourceCounterSnapshot> for ResourceCounterDto {
    fn from(value: crate::observability::ResourceCounterSnapshot) -> Self {
        Self {
            access_writeback_scheduled_total: value.access_writeback_scheduled_total,
            access_writeback_skipped_total: value.access_writeback_skipped_total,
            access_writeback_failed_total: value.access_writeback_failed_total,
        }
    }
}
