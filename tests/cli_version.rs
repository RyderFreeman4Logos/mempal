use std::process::Command;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn run_version_flag(flag: &str) -> String {
    let output = Command::new(mempal_bin())
        .arg(flag)
        .output()
        .unwrap_or_else(|error| panic!("run mempal {flag}: {error}"));
    assert!(
        output.status.success(),
        "expected mempal {flag} to exit 0, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("version stdout is UTF-8")
}

fn has_mempal_semver_prefix(output: &str) -> bool {
    let Some(version) = output.trim_end().strip_prefix("mempal ") else {
        return false;
    };
    let mut parts = version.splitn(3, '.');
    let Some(major) = parts.next() else {
        return false;
    };
    let Some(minor) = parts.next() else {
        return false;
    };
    let Some(patch_and_suffix) = parts.next() else {
        return false;
    };
    let patch_digits = patch_and_suffix
        .chars()
        .take_while(char::is_ascii_digit)
        .count();
    !major.is_empty()
        && major.chars().all(|ch| ch.is_ascii_digit())
        && !minor.is_empty()
        && minor.chars().all(|ch| ch.is_ascii_digit())
        && patch_digits > 0
}

#[test]
fn test_version_flags_print_semver() {
    let long = run_version_flag("--version");
    let short = run_version_flag("-V");

    assert_eq!(long, short, "--version and -V should match");
    assert!(
        has_mempal_semver_prefix(&long),
        "expected `mempal <semver>` output, got: {long:?}"
    );
}
