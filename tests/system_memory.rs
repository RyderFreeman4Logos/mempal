use std::fs;

use mempal::system_memory::inspect_memory_pressure_at;

#[test]
fn linux_memory_pressure_reports_available_and_cgroup_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let meminfo = tmp.path().join("meminfo");
    let cgroup = tmp.path().join("cgroup");
    fs::create_dir_all(&cgroup).expect("cgroup fixture");
    fs::write(
        &meminfo,
        "MemTotal:       16384000 kB\nMemAvailable:    4096000 kB\n",
    )
    .expect("meminfo fixture");
    fs::write(cgroup.join("memory.current"), "805306368\n").expect("current fixture");
    fs::write(cgroup.join("memory.max"), "1073741824\n").expect("max fixture");

    let snapshot = inspect_memory_pressure_at(&meminfo, &cgroup);

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
    let cgroup = tmp.path().join("cgroup");
    fs::create_dir_all(&cgroup).expect("cgroup fixture");
    fs::write(&meminfo, "MemAvailable: 1024 kB\n").expect("meminfo fixture");
    fs::write(cgroup.join("memory.current"), "4096\n").expect("current fixture");
    fs::write(cgroup.join("memory.max"), "max\n").expect("max fixture");

    let snapshot = inspect_memory_pressure_at(&meminfo, &cgroup);

    assert_eq!(snapshot.available_memory_bytes, Some(1_048_576));
    assert_eq!(snapshot.cgroup_limit_bytes, None);
    assert_eq!(snapshot.cgroup_usage_percent, None);
    assert!(!snapshot.pressure_high);
}
