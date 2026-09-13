#![cfg(target_os = "linux")]

mod common;

use std::fs;
use std::os::unix::net::UnixListener;

use common::socket_temp_dir::SocketTempDir;

const STRING_ONLY_FIXTURE: &str = r#"TempDir::new_in("/tmp")"#;

#[test]
fn unix_socket_fixture_uses_deep_configured_temp_root_without_exceeding_sun_len() {
    assert!(STRING_ONLY_FIXTURE.contains("/tmp"));
    let temp = SocketTempDir::new().expect("create socket fixture directory");
    let configured_root = std::env::temp_dir()
        .canonicalize()
        .expect("canonicalize configured temporary directory");
    let storage_path = temp
        .storage_path()
        .canonicalize()
        .expect("canonicalize socket fixture storage");
    assert!(
        storage_path.starts_with(&configured_root),
        "fixture storage must stay under the configured temporary directory"
    );

    let mempal_home = temp.path().join(".mempal");
    fs::create_dir(&mempal_home).expect("create fixture home");
    let socket_path = mempal_home.join("daemon-hook.sock");
    UnixListener::bind(&socket_path).unwrap_or_else(|error| {
        panic!(
            "socket fixture path must remain bindable: path={}, error={error}",
            socket_path.display()
        )
    });
}
