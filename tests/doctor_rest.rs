use std::net::TcpListener;
use std::process::Command;

use mockito::{Matcher, Server};
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn temp_home() -> TempDir {
    TempDir::new().expect("tempdir")
}

fn run_doctor_rest(home: &TempDir, args: &[&str]) -> std::process::Output {
    Command::new(mempal_bin())
        .arg("doctor")
        .arg("rest")
        .args(args)
        .env("HOME", home.path())
        .output()
        .expect("run mempal doctor rest")
}

#[test]
fn test_doctor_rest_reports_required_routes_and_feature_state() {
    let home = temp_home();
    let mut server = Server::new();
    let _status = server
        .mock("GET", "/api/status")
        .with_status(200)
        .with_body("{}")
        .create();
    let _search = server
        .mock("GET", "/api/search")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body("[]")
        .create();
    let _ingest = server
        .mock("POST", "/api/ingest")
        .match_body(Matcher::PartialJson(serde_json::json!({})))
        .with_status(400)
        .with_body(r#"{"error":"missing content"}"#)
        .create();
    let _timeline = server
        .mock("GET", "/api/timeline")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body("[]")
        .create();
    let _pinned = server
        .mock("GET", "/api/pinned_facts")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body("[]")
        .create();

    let output = run_doctor_rest(&home, &["--addr", &server.url(), "--format", "json"]);

    assert!(
        output.status.success(),
        "doctor rest failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor rest json");
    assert_eq!(report["rest_feature_enabled"], cfg!(feature = "rest"));
    assert_eq!(report["endpoint_reachable"], true);
    assert_eq!(report["routes"].as_array().expect("routes").len(), 5);
    assert!(
        report["routes"]
            .as_array()
            .expect("routes")
            .iter()
            .all(|route| route["available"] == true),
        "all required routes should be available: {report:#}"
    );
}

#[test]
fn test_doctor_rest_reports_server_error_routes_as_unhealthy() {
    let home = temp_home();
    let mut server = Server::new();
    let _status = server
        .mock("GET", "/api/status")
        .with_status(500)
        .with_body("status failed")
        .create();
    let _search = server
        .mock("GET", "/api/search")
        .match_query(Matcher::Any)
        .with_status(500)
        .with_body("search failed")
        .create();
    let _ingest = server
        .mock("POST", "/api/ingest")
        .match_body(Matcher::PartialJson(serde_json::json!({})))
        .with_status(500)
        .with_body("ingest failed")
        .create();
    let _timeline = server
        .mock("GET", "/api/timeline")
        .match_query(Matcher::Any)
        .with_status(500)
        .with_body("timeline failed")
        .create();
    let _pinned = server
        .mock("GET", "/api/pinned_facts")
        .match_query(Matcher::Any)
        .with_status(500)
        .with_body("pinned facts failed")
        .create();

    let output = run_doctor_rest(&home, &["--addr", &server.url(), "--format", "json"]);

    assert!(
        output.status.success(),
        "doctor rest failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor rest json");
    assert_ne!(report["status"], "ok");
    if cfg!(feature = "rest") {
        assert_eq!(report["status"], "routes_unhealthy");
    }
    assert_eq!(report["endpoint_reachable"], true);
    let routes = report["routes"].as_array().expect("routes");
    assert_eq!(routes.len(), 5);
    assert!(
        routes.iter().all(|route| route["available"] == false
            && route["http_status"] == 500
            && route["error"]
                .as_str()
                .is_some_and(|error| error.contains("server error"))),
        "server-error routes should be unavailable with route errors: {report:#}"
    );
    assert!(
        report["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|warning| warning.contains("server error"))),
        "warnings should mention server route failures: {report:#}"
    );
}

#[test]
fn test_doctor_rest_distinguishes_daemon_not_running_from_missing_rest_feature() {
    let home = temp_home();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind free port");
    let addr = listener.local_addr().expect("local addr");
    drop(listener);
    let endpoint = format!("http://{addr}");

    let output = run_doctor_rest(&home, &["--addr", &endpoint, "--format", "json"]);

    assert!(
        output.status.success(),
        "doctor rest failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor rest json");
    if cfg!(feature = "rest") {
        assert_eq!(report["status"], "daemon_not_running");
    } else {
        assert_eq!(report["status"], "missing_rest_feature");
        assert!(
            report["recommendations"]
                .as_array()
                .expect("recommendations")
                .iter()
                .any(|item| item.as_str().is_some_and(|s| s.contains("--features rest")))
        );
    }
    assert_eq!(report["endpoint_reachable"], false);
    assert_eq!(report["port"]["bind_available"], true);
}

#[test]
fn test_doctor_rest_reports_port_conflict_owner_when_unreachable() {
    let home = temp_home();
    let _listener = TcpListener::bind("127.0.0.1:0").expect("bind occupied port");
    let addr = _listener.local_addr().expect("local addr");
    let endpoint = format!("http://{addr}");

    let output = run_doctor_rest(&home, &["--addr", &endpoint, "--format", "json"]);

    assert!(
        output.status.success(),
        "doctor rest failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor rest json");
    assert_eq!(report["endpoint_reachable"], false);
    assert_eq!(report["port"]["bind_available"], false);
    if cfg!(feature = "rest") {
        assert_eq!(report["status"], "port_conflict");
    }
    #[cfg(target_os = "linux")]
    assert!(report["port"]["owner"]["pid"].as_i64().is_some());
}
