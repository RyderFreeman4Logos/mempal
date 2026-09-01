#[test]
fn systemd_unit_parses_exact_notify_service_contract() {
    let assert_contract = |unit: &str| {
        let service = unit
            .split_once("[Service]")
            .expect("systemd unit service section")
            .1
            .lines()
            .take_while(|line| !line.trim_start().starts_with('['));
        let mut directives = std::collections::BTreeMap::<&str, Vec<&str>>::new();
        for line in service {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = line.split_once('=').expect("systemd service directive");
            directives.entry(key).or_default().push(value);
        }
        assert_eq!(directives.get("Type"), Some(&vec!["notify"]));
        assert!(!directives.contains_key("TimeoutStartSec"));
        assert_eq!(directives.get("NotifyAccess"), Some(&vec!["main"]));
        assert_eq!(
            directives.get("ExecStart"),
            Some(&vec!["/usr/local/bin/mempal daemon --foreground"])
        );
    };

    let unit = include_str!("../../contrib/systemd/mempal-daemon.service");
    assert_contract(unit);
    for (original, replacement) in [
        ("Type=notify", "Type=simple"),
        ("NotifyAccess=main", "NotifyAccess=all"),
    ] {
        let mutated = unit.replace(original, replacement);
        assert_ne!(mutated, unit);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| assert_contract(&mutated)))
                .is_err(),
            "the exact service parser must reject {replacement}"
        );
    }
}
