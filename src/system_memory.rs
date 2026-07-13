//! Privacy-safe host and cgroup memory pressure diagnostics.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

const HIGH_CGROUP_USAGE_PERCENT: u64 = 75;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryPressureSnapshot {
    pub available_memory_bytes: Option<u64>,
    pub cgroup_current_bytes: Option<u64>,
    pub cgroup_limit_bytes: Option<u64>,
    pub cgroup_usage_percent: Option<u64>,
    pub pressure_high: bool,
    pub error: Option<String>,
}

pub fn inspect_memory_pressure() -> MemoryPressureSnapshot {
    inspect_memory_pressure_at(Path::new("/proc/meminfo"), Path::new("/sys/fs/cgroup"))
}

/// Inspect caller-selected paths so deterministic tests never depend on the
/// host's live cgroup or memory state.
pub fn inspect_memory_pressure_at(
    meminfo_path: &Path,
    cgroup_root: &Path,
) -> MemoryPressureSnapshot {
    let meminfo = fs::read_to_string(meminfo_path);
    let available_memory_bytes = meminfo.as_deref().ok().and_then(parse_mem_available_bytes);
    let cgroup_current = read_u64(cgroup_root.join("memory.current"));
    let cgroup_limit = fs::read_to_string(cgroup_root.join("memory.max"))
        .ok()
        .and_then(|raw| parse_cgroup_limit(&raw));
    let cgroup_usage_percent = cgroup_current
        .zip(cgroup_limit)
        .and_then(|(current, limit)| (limit > 0).then(|| current.saturating_mul(100) / limit));
    let mut errors = Vec::new();
    if meminfo.is_err() {
        errors.push("meminfo unavailable");
    }
    if cgroup_current.is_none() {
        errors.push("cgroup memory.current unavailable");
    }
    MemoryPressureSnapshot {
        available_memory_bytes,
        cgroup_current_bytes: cgroup_current,
        cgroup_limit_bytes: cgroup_limit,
        cgroup_usage_percent,
        pressure_high: cgroup_usage_percent
            .is_some_and(|percent| percent >= HIGH_CGROUP_USAGE_PERCENT),
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

fn parse_mem_available_bytes(raw: &str) -> Option<u64> {
    raw.lines().find_map(|line| {
        let value = line.strip_prefix("MemAvailable:")?.trim();
        let kib = value.strip_suffix("kB")?.trim().parse::<u64>().ok()?;
        kib.checked_mul(1024)
    })
}

fn parse_cgroup_limit(raw: &str) -> Option<u64> {
    let value = raw.trim();
    (value != "max")
        .then(|| value.parse::<u64>().ok())
        .flatten()
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
