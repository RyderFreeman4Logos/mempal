//! Privacy-safe host and cgroup memory pressure diagnostics.

use std::fs;
use std::path::{Component, Path, PathBuf};

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
    inspect_memory_pressure_at(
        Path::new("/proc/meminfo"),
        Path::new("/proc/self/cgroup"),
        Path::new("/sys/fs/cgroup"),
    )
}

/// Inspect caller-selected paths so deterministic tests never depend on the
/// host's live cgroup or memory state.
pub fn inspect_memory_pressure_at(
    meminfo_path: &Path,
    process_cgroup_path: &Path,
    cgroup_root: &Path,
) -> MemoryPressureSnapshot {
    let meminfo = fs::read_to_string(meminfo_path);
    let available_memory_bytes = meminfo.as_deref().ok().and_then(parse_mem_available_bytes);
    let process_cgroup = fs::read_to_string(process_cgroup_path);
    let cgroup_directory = process_cgroup
        .as_deref()
        .ok()
        .and_then(|raw| cgroup_v2_memory_directory(raw, cgroup_root));

    let leaf_current = cgroup_directory
        .as_ref()
        .and_then(|directory| read_u64(directory.join("memory.current")));

    // Walk ancestor cgroups to find the most constrained finite limit.
    // systemd/Kubernetes often sets memory.max on a parent while the leaf
    // shows "max". Without this, we miss real memory pressure.
    // Use the current value from the same level as the chosen limit for
    // consistent accounting scope.
    let (cgroup_limit, cgroup_current) = cgroup_directory
        .as_ref()
        .and_then(|leaf| effective_cgroup_limit(leaf, cgroup_root))
        .map(|(limit, current_at)| (Some(limit), Some(current_at)))
        .unwrap_or((None, leaf_current));
    let cgroup_usage_percent = cgroup_current
        .zip(cgroup_limit)
        .and_then(|(current, limit)| (limit > 0).then(|| current.saturating_mul(100) / limit));
    let mut errors = Vec::new();
    if meminfo.is_err() {
        errors.push("meminfo unavailable");
    }
    if process_cgroup.is_err() || cgroup_directory.is_none() {
        errors.push("process cgroup membership unavailable");
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

/// Walk from the leaf cgroup directory up to the cgroup root, returning the
/// most constrained finite `memory.max` found at any ancestor level.
fn effective_cgroup_limit(leaf: &Path, cgroup_root: &Path) -> Option<(u64, u64)> {
    let root_canon = cgroup_root.canonicalize().ok()?;
    let mut current_dir = leaf.canonicalize().ok()?;
    let mut best: Option<(u64, u64, u64)> = None; // (limit, current_at_level, pressure_ratio)
    loop {
        if let Ok(max_raw) = fs::read_to_string(current_dir.join("memory.max")) {
            if let Some(limit) = parse_cgroup_limit(&max_raw) {
                if limit > 0 {
                    // Skip this cgroup level when current usage is unreadable.
                    // Substituting zero would fabricate low pressure.
                    let Some(current_at) = read_u64(current_dir.join("memory.current")) else {
                        if current_dir == root_canon {
                            break;
                        }
                        current_dir = current_dir.parent()?.to_path_buf();
                        if !current_dir.starts_with(&root_canon) {
                            break;
                        }
                        continue;
                    };
                    let ratio = current_at
                        .saturating_mul(100)
                        .checked_div(limit)
                        .unwrap_or(0);
                    best = match best {
                        Some((prev_limit, prev_current, prev_ratio)) if prev_ratio >= ratio => {
                            Some((prev_limit, prev_current, prev_ratio))
                        }
                        _ => Some((limit, current_at, ratio)),
                    };
                }
            }
        }
        if current_dir == root_canon {
            break;
        }
        current_dir = current_dir.parent()?.to_path_buf();
        if !current_dir.starts_with(&root_canon) {
            break;
        }
    }
    best.map(|(limit, current_at, _)| (limit, current_at))
}

fn cgroup_v2_memory_directory(raw: &str, cgroup_root: &Path) -> Option<PathBuf> {
    let membership = raw.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        (hierarchy == "0" && controllers.is_empty()).then_some(path)
    })?;
    let relative = membership.strip_prefix('/')?;
    let safe = Path::new(relative)
        .components()
        .all(|component| matches!(component, Component::Normal(_)));
    safe.then(|| cgroup_root.join(relative))
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
