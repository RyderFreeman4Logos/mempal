use std::env;
use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::core::config::ConfigHandle;
use crate::core::db::CURRENT_SCHEMA_VERSION;
use crate::process_diagnostics::{DbHolderReport, inspect_db_holders};

pub const REQUIRED_MCP_TOOLS: &[&str] = &[
    "mempal_context",
    "mempal_brief",
    "mempal_phase3",
    "mempal_cowork_bus",
];

pub const PHASE3_ACTIONS: &[&str] = &[
    "guidance",
    "instrumentation_policy",
    "prepare_record",
    "capture",
    "evaluator_advise",
    "default_proposal",
    "rollback_control",
    "check_record",
    "record_checked",
    "review",
    "readiness",
    "analytics",
    "record",
    "list",
    "stats",
    "gate",
    "research_validate_plan",
    "research_ingest_plan",
];

pub const COWORK_BUS_ACTIONS: &[&str] = &[
    "register",
    "list",
    "send",
    "broadcast",
    "drain",
    "events",
    "deliveries",
    "ack",
    "heartbeat",
    "channel_set",
    "channel_list",
    "channel_send",
    "tmux_peek",
    "doctor",
    "session_create",
    "session_list",
    "session_status",
    "session_close",
    "handoff",
    "capture",
];

pub const REQUIRED_REST_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/status"),
    ("GET", "/api/search"),
    ("POST", "/api/ingest"),
    ("GET", "/api/timeline"),
    ("GET", "/api/pinned_facts"),
];

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub current_version: String,
    pub supported_schema_version: u32,
    pub db: DoctorDbReport,
    pub db_holders: DbHolderReport,
    pub install: DoctorInstallReport,
    pub restart_required_config_changes: Vec<String>,
    pub warnings: Vec<String>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorDbReport {
    pub path: String,
    pub exists: bool,
    pub schema_version: Option<u32>,
    pub compatible: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorInstallReport {
    pub current_exe: Option<String>,
    pub path_mempal: Option<String>,
    pub path_matches_current_exe: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestDoctorReport {
    pub rest_feature_enabled: bool,
    pub endpoint: String,
    pub endpoint_reachable: bool,
    pub status: String,
    pub routes: Vec<RestRouteReport>,
    pub port: Option<RestPortReport>,
    pub warnings: Vec<String>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestRouteReport {
    pub method: String,
    pub path: String,
    pub available: bool,
    pub http_status: Option<u16>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestPortReport {
    pub addr: String,
    pub bind_available: bool,
    pub error: Option<String>,
    pub owner: Option<RestPortOwner>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RestPortOwner {
    pub pid: i32,
    pub command: String,
}

pub fn build_doctor_report(db_path: &Path) -> DoctorReport {
    let db = inspect_db(db_path);
    let db_holders = inspect_db_holders(db_path);
    let install = inspect_install();
    let restart_required_config_changes = ConfigHandle::restart_required_pending();
    let mut warnings = Vec::new();
    let mut recommendations = vec![
        "Run `mempal doctor --format json` after installing or upgrading mempal.".to_string(),
        "If PATH points at an older binary, restart the MCP client after updating PATH."
            .to_string(),
    ];

    if db.exists && !db.compatible {
        warnings.push(format!(
            "database schema is not compatible with this binary: found {:?}, supported {}",
            db.schema_version, CURRENT_SCHEMA_VERSION
        ));
        recommendations
            .push("Install a mempal binary that supports this palace.db schema.".to_string());
    }
    if let Some(error) = db.error.as_deref() {
        warnings.push(format!("database schema could not be inspected: {error}"));
    }
    for change in &restart_required_config_changes {
        warnings.push(format!("config change pending restart: {change}"));
    }
    push_db_holder_warnings(&mut warnings, &mut recommendations, &db_holders);
    if install.path_matches_current_exe == Some(false) {
        warnings
            .push("PATH resolves mempal to a different executable than this process".to_string());
        recommendations
            .push("Check `which mempal` and restart long-lived MCP clients.".to_string());
    }
    if install.path_mempal.is_none() {
        warnings.push("PATH does not contain a mempal executable".to_string());
    }

    DoctorReport {
        current_version: env!("CARGO_PKG_VERSION").to_string(),
        supported_schema_version: CURRENT_SCHEMA_VERSION,
        db,
        db_holders,
        install,
        restart_required_config_changes,
        warnings,
        recommendations,
    }
}

pub async fn build_rest_doctor_report(endpoint: &str, db_path: Option<&Path>) -> RestDoctorReport {
    let endpoint = normalize_rest_endpoint(endpoint);
    let port = inspect_rest_port(&endpoint);
    let routes = probe_rest_routes(&endpoint).await;
    let endpoint_reachable = routes.iter().any(|route| route.http_status.is_some());
    let missing_routes = routes
        .iter()
        .filter(|route| is_missing_rest_route(route))
        .map(|route| format!("{} {}", route.method, route.path))
        .collect::<Vec<_>>();
    let unhealthy_routes = routes
        .iter()
        .filter_map(unhealthy_rest_route_detail)
        .collect::<Vec<_>>();

    let mut warnings = Vec::new();
    let mut recommendations = Vec::new();

    if !cfg!(feature = "rest") {
        warnings.push(
            "this mempal binary was built without the `rest` feature; it cannot serve REST"
                .to_string(),
        );
        recommendations.push(suggest_rest_install_command());
    }

    match (&port, endpoint_reachable) {
        (Some(port), false) if port.bind_available => {
            warnings.push(format!(
                "REST endpoint {endpoint} is unreachable and {} is free; daemon is not running, REST is disabled, or the daemon was built without REST",
                port.addr
            ));
            recommendations.push(
                "Start a REST-enabled daemon with `[api].enabled = true`, then rerun `mempal doctor rest`."
                    .to_string(),
            );
        }
        (Some(port), false) if !port.bind_available => {
            warnings.push(format!(
                "REST endpoint {endpoint} is unreachable but {} is already bound",
                port.addr
            ));
            if let Some(owner) = &port.owner {
                warnings.push(format!(
                    "REST port owner: pid={} command={}",
                    owner.pid, owner.command
                ));
            }
            recommendations.push(
                "Use a profile-specific local `[api].addr`, for example `127.0.0.1:3081`, or stop the conflicting process."
                    .to_string(),
            );
        }
        _ => {}
    }

    if endpoint_reachable && !missing_routes.is_empty() {
        warnings.push(format!(
            "REST endpoint is reachable but required route(s) are missing: {}",
            missing_routes.join(", ")
        ));
        recommendations.push(
            "Reinstall mempal with `--features rest` and restart the daemon serving this endpoint."
                .to_string(),
        );
    }
    if endpoint_reachable && !unhealthy_routes.is_empty() {
        warnings.push(format!(
            "REST endpoint is reachable but required route(s) returned unhealthy response(s): {}",
            unhealthy_routes.join(", ")
        ));
        recommendations.push(
            "Inspect the REST daemon logs and fix the failing route handler or database/profile configuration."
                .to_string(),
        );
    }

    if let (Some(db_path), Some(port)) = (db_path, &port)
        && endpoint_reachable
        && let Some(owner) = &port.owner
    {
        let holders = inspect_db_holders(db_path);
        let owner_holds_current_db = holders.holders.iter().any(|holder| holder.pid == owner.pid);
        if db_path.exists() && !owner_holds_current_db {
            warnings.push(format!(
                "REST port owner pid={} was not observed holding configured DB {}; verify this endpoint is not another mempal profile",
                owner.pid,
                db_path.display()
            ));
            recommendations.push(
                "For multiple daemon profiles, assign each profile a distinct local `[api].addr`."
                    .to_string(),
            );
        }
    }

    let status = if !cfg!(feature = "rest") {
        "missing_rest_feature"
    } else if !endpoint_reachable && port.as_ref().is_some_and(|p| p.bind_available) {
        "daemon_not_running"
    } else if !endpoint_reachable {
        "port_conflict"
    } else if !unhealthy_routes.is_empty() {
        "routes_unhealthy"
    } else if !missing_routes.is_empty() {
        "routes_missing"
    } else {
        "ok"
    };

    RestDoctorReport {
        rest_feature_enabled: cfg!(feature = "rest"),
        endpoint,
        endpoint_reachable,
        status: status.to_string(),
        routes,
        port,
        warnings,
        recommendations,
    }
}

fn push_db_holder_warnings(
    warnings: &mut Vec<String>,
    recommendations: &mut Vec<String>,
    db_holders: &DbHolderReport,
) {
    if let Some(error) = db_holders.error.as_deref() {
        warnings.push(format!(
            "database holder processes could not be inspected: {error}"
        ));
    }
    if db_holders.stale_mcp_server_count > 0 {
        warnings.push(format!(
            "{} stale mempal MCP server process(es) hold the database open",
            db_holders.stale_mcp_server_count
        ));
        recommendations.push(
            "Restart stale MCP clients after confirming they are not the active session."
                .to_string(),
        );
    }
    if db_holders.orphan_daemon_count > 0 {
        warnings.push(format!(
            "{} orphan daemon process(es) hold the database open",
            db_holders.orphan_daemon_count
        ));
        recommendations.push("Run `mempal daemon status` and `mempal daemon reap`.".to_string());
    }
    if db_holders.extra_holder_count > 0 {
        warnings.push(format!(
            "{} extra process(es) hold the database open",
            db_holders.extra_holder_count
        ));
    }
}

fn inspect_db(db_path: &Path) -> DoctorDbReport {
    if !db_path.exists() {
        return DoctorDbReport {
            path: db_path.display().to_string(),
            exists: false,
            schema_version: None,
            compatible: true,
            error: None,
        };
    }

    match read_schema_version_read_only(db_path) {
        Ok(schema_version) => DoctorDbReport {
            path: db_path.display().to_string(),
            exists: true,
            schema_version: Some(schema_version),
            compatible: schema_version <= CURRENT_SCHEMA_VERSION,
            error: None,
        },
        Err(error) => DoctorDbReport {
            path: db_path.display().to_string(),
            exists: true,
            schema_version: None,
            compatible: false,
            error: Some(error),
        },
    }
}

fn read_schema_version_read_only(db_path: &Path) -> Result<u32, String> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())?;
    conn.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .map_err(|error| error.to_string())
}

fn inspect_install() -> DoctorInstallReport {
    let current_exe = env::current_exe().ok();
    let path_mempal = find_path_executable("mempal");
    let path_matches_current_exe = match (current_exe.as_ref(), path_mempal.as_ref()) {
        (Some(current), Some(path_mempal)) => Some(paths_match(current, path_mempal)),
        (_, Some(_)) => None,
        _ => None,
    };

    DoctorInstallReport {
        current_exe: current_exe.map(|path| path.display().to_string()),
        path_mempal: path_mempal.map(|path| path.display().to_string()),
        path_matches_current_exe,
    }
}

fn find_path_executable(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn paths_match(left: &Path, right: &Path) -> bool {
    let left = left.canonicalize().unwrap_or_else(|_| left.to_path_buf());
    let right = right.canonicalize().unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn normalize_rest_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

async fn probe_rest_routes(endpoint: &str) -> Vec<RestRouteReport> {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return REQUIRED_REST_ROUTES
                .iter()
                .map(|(method, path)| RestRouteReport {
                    method: (*method).to_string(),
                    path: (*path).to_string(),
                    available: false,
                    http_status: None,
                    error: Some(error.to_string()),
                })
                .collect();
        }
    };

    let mut reports = Vec::new();
    for (method, path) in REQUIRED_REST_ROUTES {
        let url = route_probe_url(endpoint, path);
        let request = match *method {
            "GET" => client.get(url),
            "POST" => client.post(url).json(&serde_json::json!({})),
            other => {
                reports.push(RestRouteReport {
                    method: other.to_string(),
                    path: (*path).to_string(),
                    available: false,
                    http_status: None,
                    error: Some("unsupported route probe method".to_string()),
                });
                continue;
            }
        };
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let (available, error) = classify_rest_route_probe(method, path, status);
                reports.push(RestRouteReport {
                    method: (*method).to_string(),
                    path: (*path).to_string(),
                    available,
                    http_status: Some(status.as_u16()),
                    error,
                });
            }
            Err(error) => reports.push(RestRouteReport {
                method: (*method).to_string(),
                path: (*path).to_string(),
                available: false,
                http_status: None,
                error: Some(error.to_string()),
            }),
        }
    }
    reports
}

fn classify_rest_route_probe(
    method: &str,
    path: &str,
    status: reqwest::StatusCode,
) -> (bool, Option<String>) {
    if status.is_success() || is_expected_ingest_validation(method, path, status) {
        return (true, None);
    }
    if matches!(
        status,
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
    ) {
        return (false, None);
    }
    if status.is_server_error() {
        return (
            false,
            Some(format!("server error HTTP {}", status.as_u16())),
        );
    }
    (
        false,
        Some(format!("unexpected HTTP status {}", status.as_u16())),
    )
}

fn is_expected_ingest_validation(method: &str, path: &str, status: reqwest::StatusCode) -> bool {
    method == "POST"
        && path == "/api/ingest"
        && matches!(
            status,
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNPROCESSABLE_ENTITY
        )
}

fn is_missing_rest_route(route: &RestRouteReport) -> bool {
    matches!(route.http_status, Some(404 | 405))
}

fn unhealthy_rest_route_detail(route: &RestRouteReport) -> Option<String> {
    if route.available || is_missing_rest_route(route) {
        return None;
    }
    let status = route
        .http_status
        .map(|status| format!("HTTP {status}"))
        .unwrap_or_else(|| "no HTTP response".to_string());
    let detail = route
        .error
        .as_deref()
        .map(|error| format!("{status} ({error})"))
        .unwrap_or(status);
    Some(format!("{} {} {detail}", route.method, route.path))
}

fn route_probe_url(endpoint: &str, path: &str) -> String {
    match path {
        "/api/search" => format!("{endpoint}{path}?q=mempal-doctor-rest&top_k=1"),
        "/api/timeline" | "/api/pinned_facts" => format!("{endpoint}{path}?limit=1"),
        _ => format!("{endpoint}{path}"),
    }
}

fn inspect_rest_port(endpoint: &str) -> Option<RestPortReport> {
    let addr = endpoint_socket_addr(endpoint)?;
    match TcpListener::bind(addr) {
        Ok(listener) => {
            drop(listener);
            Some(RestPortReport {
                addr: addr.to_string(),
                bind_available: true,
                error: None,
                owner: None,
            })
        }
        Err(error) => Some(RestPortReport {
            addr: addr.to_string(),
            bind_available: false,
            error: Some(error.to_string()),
            owner: inspect_port_owner(addr),
        }),
    }
}

fn endpoint_socket_addr(endpoint: &str) -> Option<SocketAddr> {
    let url = reqwest::Url::parse(endpoint).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    (host, port)
        .to_socket_addrs()
        .ok()?
        .find(|addr| addr.ip().is_loopback())
        .or_else(|| (host, port).to_socket_addrs().ok()?.next())
}

fn inspect_port_owner(addr: SocketAddr) -> Option<RestPortOwner> {
    #[cfg(target_os = "linux")]
    {
        inspect_port_owner_in_proc(addr.port(), Path::new("/proc"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = addr;
        None
    }
}

#[cfg(target_os = "linux")]
fn inspect_port_owner_in_proc(port: u16, proc_root: &Path) -> Option<RestPortOwner> {
    let inodes = listening_socket_inodes(port, proc_root);
    if inodes.is_empty() {
        return None;
    }
    let entries = std::fs::read_dir(proc_root).ok()?;
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let fd_dir = entry.path().join("fd");
        let Ok(fds) = std::fs::read_dir(fd_dir) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            if let Some(inode) = target
                .strip_prefix("socket:[")
                .and_then(|value| value.strip_suffix(']'))
                && inodes.iter().any(|known| known == inode)
            {
                return Some(RestPortOwner {
                    pid,
                    command: read_proc_command(proc_root, pid),
                });
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn listening_socket_inodes(port: u16, proc_root: &Path) -> Vec<String> {
    [proc_root.join("net/tcp"), proc_root.join("net/tcp6")]
        .into_iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .flat_map(|content| {
            content
                .lines()
                .filter_map(move |line| listening_socket_inode_from_line(line, port))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn listening_socket_inode_from_line(line: &str, port: u16) -> Option<String> {
    let columns = line.split_whitespace().collect::<Vec<_>>();
    let local_address = columns.get(1)?;
    let state = columns.get(3)?;
    if *state != "0A" {
        return None;
    }
    let port_hex = local_address.split_once(':')?.1;
    let parsed_port = u16::from_str_radix(port_hex, 16).ok()?;
    if parsed_port != port {
        return None;
    }
    columns.get(9).map(|inode| (*inode).to_string())
}

#[cfg(target_os = "linux")]
fn read_proc_command(proc_root: &Path, pid: i32) -> String {
    let comm = proc_root.join(pid.to_string()).join("comm");
    std::fs::read_to_string(comm)
        .map(|value| value.trim().to_string())
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn suggest_rest_install_command() -> String {
    let root = env::current_exe()
        .ok()
        .and_then(|path| path.parent().and_then(Path::parent).map(Path::to_path_buf))
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<install-root>".to_string());
    format!(
        "Install REST support with `cargo install --path . --locked --features rest --force --root {root}` and restart the daemon."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_holder_warnings_report_extra_holders_alongside_specific_roles() {
        let report = DbHolderReport {
            db_path: "/tmp/palace.db".to_string(),
            holder_count: 3,
            extra_holder_count: 1,
            stale_mcp_server_count: 1,
            orphan_daemon_count: 1,
            error: None,
            holders: Vec::new(),
        };
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();

        push_db_holder_warnings(&mut warnings, &mut recommendations, &report);

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("stale mempal MCP server"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("orphan daemon"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("extra process"))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_listening_socket_inode_from_proc_net_tcp_line() {
        let line = "0: 0100007F:0C08 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 4242 1 0000000000000000";

        assert_eq!(
            listening_socket_inode_from_line(line, 3080),
            Some("4242".to_string())
        );
        assert_eq!(listening_socket_inode_from_line(line, 3081), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_inspect_port_owner_in_proc_maps_socket_inode_to_process() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let proc_root = tmp.path();
        std::fs::create_dir_all(proc_root.join("net")).expect("create net");
        std::fs::write(
            proc_root.join("net/tcp"),
            "sl local_address rem_address st tx_queue rx_queue tr tm->when retrnsmt uid timeout inode\n 0: 0100007F:0C08 00000000:0000 0A 00000000:00000000 00:00000000 00000000 1000 0 4242 1 0\n",
        )
        .expect("write tcp");
        std::fs::write(proc_root.join("net/tcp6"), "").expect("write tcp6");
        std::fs::create_dir_all(proc_root.join("123/fd")).expect("create fd");
        std::fs::write(proc_root.join("123/comm"), "mempal\n").expect("write comm");
        #[cfg(unix)]
        std::os::unix::fs::symlink("socket:[4242]", proc_root.join("123/fd/7"))
            .expect("symlink socket");

        let owner = inspect_port_owner_in_proc(3080, proc_root).expect("owner");

        assert_eq!(
            owner,
            RestPortOwner {
                pid: 123,
                command: "mempal".to_string(),
            }
        );
    }
}
