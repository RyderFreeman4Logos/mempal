use std::fs;

use mempal::system_memory::inspect_memory_pressure_at;

#[test]
fn linux_memory_pressure_reports_available_and_cgroup_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let meminfo = tmp.path().join("meminfo");
    let process_cgroup = tmp.path().join("self.cgroup");
    let cgroup = tmp.path().join("cgroup");
    let process_group = cgroup.join("user.slice/mempal.scope");
    fs::create_dir_all(&process_group).expect("cgroup fixture");
    fs::write(
        &meminfo,
        "MemTotal:       16384000 kB\nMemAvailable:    4096000 kB\n",
    )
    .expect("meminfo fixture");
    fs::write(&process_cgroup, "0::/user.slice/mempal.scope\n").expect("membership fixture");
    fs::write(cgroup.join("memory.current"), "1\n").expect("root current fixture");
    fs::write(cgroup.join("memory.max"), "max\n").expect("root max fixture");
    fs::write(process_group.join("memory.current"), "805306368\n").expect("current fixture");
    fs::write(process_group.join("memory.max"), "1073741824\n").expect("max fixture");

    let snapshot = inspect_memory_pressure_at(&meminfo, &process_cgroup, &cgroup);

    assert_eq!(snapshot.available_memory_bytes, Some(4_194_304_000));
    assert_eq!(snapshot.cgroup_current_bytes, Some(805_306_368));
    assert_eq!(snapshot.cgroup_limit_bytes, Some(1_073_741_824));
    assert_eq!(snapshot.cgroup_usage_percent, Some(75));
    assert!(snapshot.pressure_high);
    assert!(snapshot.error.is_none());
}

#[test]
fn unlimited_cgroup_is_reported_without_fabricated_ratio() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let meminfo = tmp.path().join("meminfo");
    let process_cgroup = tmp.path().join("self.cgroup");
    let cgroup = tmp.path().join("cgroup");
    fs::create_dir_all(&cgroup).expect("cgroup fixture");
    fs::write(&meminfo, "MemAvailable: 1024 kB\n").expect("meminfo fixture");
    fs::write(&process_cgroup, "0::/\n").expect("membership fixture");
    fs::write(cgroup.join("memory.current"), "4096\n").expect("current fixture");
    fs::write(cgroup.join("memory.max"), "max\n").expect("max fixture");

    let snapshot = inspect_memory_pressure_at(&meminfo, &process_cgroup, &cgroup);

    assert_eq!(snapshot.available_memory_bytes, Some(1_048_576));
    assert_eq!(snapshot.cgroup_limit_bytes, None);
    assert_eq!(snapshot.cgroup_usage_percent, None);
    assert!(!snapshot.pressure_high);
}

#[test]
fn unsafe_or_missing_membership_never_falls_back_to_cgroup_root() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let meminfo = tmp.path().join("meminfo");
    let process_cgroup = tmp.path().join("self.cgroup");
    let cgroup = tmp.path().join("cgroup");
    fs::create_dir_all(&cgroup).expect("cgroup fixture");
    fs::write(&meminfo, "MemAvailable: 1024 kB\n").expect("meminfo fixture");
    fs::write(&process_cgroup, "0::/../../host\n").expect("membership fixture");
    fs::write(cgroup.join("memory.current"), "4096\n").expect("root current fixture");
    fs::write(cgroup.join("memory.max"), "8192\n").expect("root max fixture");

    let snapshot = inspect_memory_pressure_at(&meminfo, &process_cgroup, &cgroup);

    assert_eq!(snapshot.cgroup_current_bytes, None);
    assert_eq!(snapshot.cgroup_limit_bytes, None);
    assert_eq!(snapshot.cgroup_usage_percent, None);
    assert!(
        snapshot
            .error
            .as_deref()
            .is_some_and(|error| error.contains("process cgroup membership unavailable"))
    );
}
