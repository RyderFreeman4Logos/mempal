use std::net::{SocketAddr, TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, fs, io};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;

use crate::core::config::{Config, ConfigHandle};
use crate::core::db::CURRENT_SCHEMA_VERSION;
use crate::core::design_insights::{
    design_insights_table_exists, unresolved_design_insight_summary,
};
use crate::core::queue::{QueueStats, queue_stats_readonly};
use crate::core::remote_calls::{
    RemoteCallService, endpoint_policy_display_label, endpoint_policy_global_runtime_error,
    endpoint_policy_runtime_error,
};
use crate::daemon_status::{DaemonEmbedderRuntimeStatus, read_embedder_status};
use crate::process_diagnostics::{
    DbHolderReport, ProcessMemoryReport, inspect_db_holders, inspect_process_memory,
};

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
    pub daemon: DoctorDaemonReport,
    pub install: DoctorInstallReport,
    pub embedding: DoctorEmbeddingReport,
    pub availability: DoctorAvailabilityReport,
    pub design_insights: DoctorDesignInsightReport,
    pub restart_required_config_changes: Vec<String>,
    pub warnings: Vec<String>,
    pub recommendations: Vec<String>,
}

/// Pending work at or above this count is an availability outage when the daemon is down.
pub const DAEMON_OUTAGE_PENDING_QUEUE_THRESHOLD: u64 = 100;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorAvailabilitySeverity {
    Normal,
    High,
    Unavailable,
}

impl DoctorAvailabilitySeverity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::High => "high",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorAvailabilitySignal {
    DaemonDownLargePendingQueue,
    DiagnosticInputsUnavailable,
}

impl DoctorAvailabilitySignal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DaemonDownLargePendingQueue => "daemon_down_large_pending_queue",
            Self::DiagnosticInputsUnavailable => "diagnostic_inputs_unavailable",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorAvailabilityUnavailableReason {
    Config,
    QueueStats,
    DaemonIdentity,
}

impl DoctorAvailabilityUnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::QueueStats => "queue_stats",
            Self::DaemonIdentity => "daemon_identity",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum DoctorAvailabilityObservation<T> {
    Known(T),
    Unavailable(DoctorAvailabilityUnavailableReason),
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorAvailabilityReport {
    pub severity: DoctorAvailabilitySeverity,
    pub signal: Option<DoctorAvailabilitySignal>,
    pub daemon_running: Option<bool>,
    pub pending_queue: Option<u64>,
    pub pending_queue_threshold: u64,
    pub unavailable_reasons: Vec<DoctorAvailabilityUnavailableReason>,
}

impl DoctorAvailabilityReport {
    pub fn warning_message(&self) -> Option<String> {
        match self.signal {
            Some(DoctorAvailabilitySignal::DaemonDownLargePendingQueue) => Some(format!(
                "daemon is down with {} pending embedding/hook queue item(s) (threshold {}); queued work cannot drain",
                self.pending_queue.unwrap_or_default(),
                self.pending_queue_threshold
            )),
            Some(DoctorAvailabilitySignal::DiagnosticInputsUnavailable) => Some(format!(
                "availability is unavailable because diagnostic input(s) could not be observed: {}",
                self.unavailable_reasons
                    .iter()
                    .map(|reason| reason.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )),
            None => None,
        }
    }
}

pub fn daemon_outage_queue_availability(
    daemon: DoctorAvailabilityObservation<bool>,
    queue: DoctorAvailabilityObservation<u64>,
) -> DoctorAvailabilityReport {
    let mut unavailable_reasons = Vec::new();
    let daemon_running = match daemon {
        DoctorAvailabilityObservation::Known(running) => Some(running),
        DoctorAvailabilityObservation::Unavailable(reason) => {
            unavailable_reasons.push(reason);
            None
        }
    };
    let pending_queue = match queue {
        DoctorAvailabilityObservation::Known(pending) => Some(pending),
        DoctorAvailabilityObservation::Unavailable(reason) => {
            unavailable_reasons.push(reason);
            None
        }
    };
    let signal = if unavailable_reasons.is_empty() {
        (daemon_running == Some(false)
            && pending_queue
                .is_some_and(|pending| pending >= DAEMON_OUTAGE_PENDING_QUEUE_THRESHOLD))
        .then_some(DoctorAvailabilitySignal::DaemonDownLargePendingQueue)
    } else {
        Some(DoctorAvailabilitySignal::DiagnosticInputsUnavailable)
    };
    DoctorAvailabilityReport {
        severity: match signal {
            Some(DoctorAvailabilitySignal::DaemonDownLargePendingQueue) => {
                DoctorAvailabilitySeverity::High
            }
            Some(DoctorAvailabilitySignal::DiagnosticInputsUnavailable) => {
                DoctorAvailabilitySeverity::Unavailable
            }
            None => DoctorAvailabilitySeverity::Normal,
        },
        signal,
        daemon_running,
        pending_queue,
        pending_queue_threshold: DAEMON_OUTAGE_PENDING_QUEUE_THRESHOLD,
        unavailable_reasons,
    }
}

fn daemon_outage_queue_recovery_guidance() -> &'static str {
    "To recover, start the daemon, confirm queue claim/drain progress with `mempal status` or `mempal daemon status`, then handle terminal failed operations with `mempal queue failed`; do not edit the database manually."
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DoctorEmbeddingReport {
    pub backend: String,
    pub model: Option<String>,
    pub pool_capacity: usize,
    pub runtime_status_source: String,
    pub runtime_status_available: bool,
    pub degraded: bool,
    pub block_writes_when_degraded: bool,
    pub write_refused: bool,
    pub fail_count: u64,
    pub last_error: Option<String>,
    pub last_success_at_unix_ms: Option<u64>,
    pub endpoints: Vec<DoctorEmbeddingEndpointReport>,
    pub queue: DoctorEmbeddingQueueReport,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorEmbeddingEndpointReport {
    pub id: String,
    pub backend: String,
    pub base_url: String,
    pub model: String,
    pub priority: i32,
    pub retry_interval_secs: u64,
    pub request_timeout_secs: u64,
    pub max_concurrent: usize,
    pub dimensions: usize,
    pub cooldown_remaining_secs: Option<u64>,
    pub cooldown_until_unix_ms: Option<u64>,
    pub last_failure_at_unix_ms: Option<u64>,
    pub last_success_at_unix_ms: Option<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DoctorEmbeddingQueueReport {
    pub pending: u64,
    pub claimed: u64,
    pub failed: u64,
    pub failed_retryable: u64,
    pub failed_terminal: u64,
    pub failed_retryable_embed: u64,
    pub failed_retryable_llm: u64,
    pub last_auto_requeue_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DoctorDaemonReport {
    pub pid: Option<i32>,
    pub running: bool,
    pub embedder: Option<DaemonEmbedderRuntimeStatus>,
    pub process: Option<ProcessMemoryReport>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DoctorDesignInsightReport {
    pub schema_available: bool,
    pub open_total: u64,
    pub high_value_open: u64,
    pub error: Option<String>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub degraded_reasons: Vec<String>,
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
    build_doctor_report_with_daemon_status(db_path, None)
}

pub fn build_doctor_report_with_daemon_status(
    db_path: &Path,
    daemon_status: Option<&Value>,
) -> DoctorReport {
    let db = inspect_db(db_path);
    let db_holders = inspect_db_holders(db_path);
    let (daemon, daemon_observation) = inspect_daemon(db_path);
    let install = inspect_install();
    let config = Config::load().ok();
    let embedding = build_embedding_report(config.as_ref(), db_path, daemon_status);
    let availability =
        daemon_outage_queue_availability(daemon_observation, embedding.queue_pending);
    let design_insights = inspect_design_insights(db_path);
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
    if design_insights.high_value_open > 0 {
        warnings.push(format!(
            "{} unresolved high-value design insight(s) need draining",
            design_insights.high_value_open
        ));
        recommendations
            .push("Run `mempal insight list --status open --min-priority 4`.".to_string());
    }
    if let Some(error) = design_insights.error.as_deref() {
        warnings.push(format!("design insights could not be inspected: {error}"));
    }
    for change in &restart_required_config_changes {
        warnings.push(format!("config change pending restart: {change}"));
    }
    push_db_holder_warnings(&mut warnings, &mut recommendations, &db_holders);
    push_daemon_warnings(&mut warnings, &mut recommendations, &daemon);
    push_daemon_outage_queue_guidance(&mut warnings, &mut recommendations, &availability);
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
        daemon,
        install,
        embedding: embedding.report,
        availability,
        design_insights,
        restart_required_config_changes,
        warnings,
        recommendations,
    }
}

fn inspect_design_insights(db_path: &Path) -> DoctorDesignInsightReport {
    if !db_path.exists() {
        return DoctorDesignInsightReport::default();
    }
    let conn = match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        Ok(conn) => conn,
        Err(error) => {
            return DoctorDesignInsightReport {
                error: Some(error.to_string()),
                ..DoctorDesignInsightReport::default()
            };
        }
    };
    if !design_insights_table_exists(&conn) {
        return DoctorDesignInsightReport::default();
    }
    match unresolved_design_insight_summary(&conn) {
        Ok(summary) => DoctorDesignInsightReport {
            schema_available: true,
            open_total: summary.open_total,
            high_value_open: summary.high_value_open,
            error: None,
        },
        Err(error) => DoctorDesignInsightReport {
            schema_available: true,
            error: Some(error.to_string()),
            ..DoctorDesignInsightReport::default()
        },
    }
}

struct DoctorEmbeddingBuild {
    report: DoctorEmbeddingReport,
    queue_pending: DoctorAvailabilityObservation<u64>,
}

fn build_embedding_report(
    config: Option<&Config>,
    db_path: &Path,
    daemon_status: Option<&Value>,
) -> DoctorEmbeddingBuild {
    let Some(config) = config else {
        return DoctorEmbeddingBuild {
            report: DoctorEmbeddingReport {
                runtime_status_source: "unavailable".to_string(),
                ..DoctorEmbeddingReport::default()
            },
            queue_pending: DoctorAvailabilityObservation::Unavailable(
                DoctorAvailabilityUnavailableReason::Config,
            ),
        };
    };
    let embed_status = crate::embed::global_embed_status();
    let embed_snapshot = embed_status.snapshot();
    let endpoint_configs = config.embed.effective_endpoints().unwrap_or_default();
    let (queue, fail_count, queue_pending) = match queue_stats_readonly(db_path) {
        Ok(stats) => {
            let fail_count =
                crate::core::queue::failure_headline_count(embed_snapshot.fail_count, &stats);
            let pending = stats.pending;
            (
                queue_report_from_stats(stats),
                fail_count,
                DoctorAvailabilityObservation::Known(pending),
            )
        }
        Err(_) => (
            DoctorEmbeddingQueueReport::default(),
            embed_snapshot.fail_count,
            DoctorAvailabilityObservation::Unavailable(
                DoctorAvailabilityUnavailableReason::QueueStats,
            ),
        ),
    };
    let last_error = sanitize_runtime_error(endpoint_policy_global_runtime_error(
        &config.privacy.remote_calls,
        RemoteCallService::Embedding,
        endpoint_configs
            .iter()
            .map(|endpoint| endpoint.base_url.as_str()),
        embed_snapshot.last_error,
    ));
    let endpoint_runtime = embed_status
        .endpoint_runtime_snapshots()
        .into_iter()
        .map(|snapshot| (snapshot.id.clone(), snapshot))
        .collect::<std::collections::BTreeMap<_, _>>();
    let endpoints = endpoint_configs
        .into_iter()
        .map(|endpoint| {
            let runtime = endpoint_runtime.get(&endpoint.id);
            let last_error = sanitize_runtime_error(runtime.and_then(|state| {
                endpoint_policy_runtime_error(
                    &config.privacy.remote_calls,
                    RemoteCallService::Embedding,
                    &endpoint.base_url,
                    state.last_error.clone(),
                )
            }));
            DoctorEmbeddingEndpointReport {
                id: endpoint.id,
                backend: endpoint.backend,
                base_url: endpoint_policy_display_label(
                    &config.privacy.remote_calls,
                    RemoteCallService::Embedding,
                    &endpoint.base_url,
                ),
                model: endpoint.model,
                priority: endpoint.priority,
                retry_interval_secs: endpoint.retry_interval_secs,
                request_timeout_secs: endpoint.request_timeout_secs,
                max_concurrent: endpoint.max_concurrent,
                dimensions: endpoint.dimensions,
                cooldown_remaining_secs: runtime.and_then(|state| state.cooldown_remaining_secs),
                cooldown_until_unix_ms: runtime.and_then(|state| state.cooldown_until_unix_ms),
                last_failure_at_unix_ms: runtime.and_then(|state| state.last_failure_at_unix_ms),
                last_success_at_unix_ms: runtime.and_then(|state| state.last_success_at_unix_ms),
                last_error,
            }
        })
        .collect();
    let block_writes_when_degraded = config.embed.degradation.block_writes_when_degraded;
    let write_refused = embed_snapshot.degraded && block_writes_when_degraded;
    let mut report = DoctorEmbeddingReport {
        backend: config.embed.backend.clone(),
        model: config.embed.effective_model_summary(),
        pool_capacity: config.embed.pool_capacity(),
        runtime_status_source: "unavailable".to_string(),
        runtime_status_available: false,
        degraded: embed_snapshot.degraded,
        block_writes_when_degraded,
        write_refused,
        fail_count,
        last_error,
        last_success_at_unix_ms: embed_snapshot.last_success_at_unix_ms,
        endpoints,
        queue,
    };
    overlay_daemon_embedding_status(&mut report, daemon_status);
    DoctorEmbeddingBuild {
        report,
        queue_pending,
    }
}

fn overlay_daemon_embedding_status(
    report: &mut DoctorEmbeddingReport,
    daemon_status: Option<&Value>,
) {
    let Some(embed_status) = daemon_status.and_then(|status| status.get("embed_status")) else {
        return;
    };
    report.runtime_status_source = "daemon_rest".to_string();
    report.runtime_status_available = true;
    if let Some(degraded) = embed_status.get("degraded").and_then(Value::as_bool) {
        report.degraded = degraded;
    }
    if let Some(block_writes_when_degraded) = embed_status
        .get("block_writes_when_degraded")
        .and_then(Value::as_bool)
    {
        report.block_writes_when_degraded = block_writes_when_degraded;
    }
    if let Some(write_refused) = embed_status.get("write_refused").and_then(Value::as_bool) {
        report.write_refused = write_refused;
    }
    if let Some(fail_count) = embed_status.get("fail_count").and_then(Value::as_u64) {
        report.fail_count = fail_count;
    }
    report.last_error = embed_status
        .get("last_error")
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .and_then(|error| sanitize_runtime_error(Some(error)));
    report.last_success_at_unix_ms = embed_status
        .get("last_success_at_unix_ms")
        .and_then(Value::as_u64);
}

fn sanitize_runtime_error(error: Option<String>) -> Option<String> {
    error.map(|message| crate::core::config::scrub_runtime_diagnostic_text(&message))
}

fn queue_report_from_stats(stats: QueueStats) -> DoctorEmbeddingQueueReport {
    DoctorEmbeddingQueueReport {
        pending: stats.pending,
        claimed: stats.claimed,
        failed: stats.failed,
        failed_retryable: stats.failed_retryable,
        failed_terminal: stats.failed_terminal,
        failed_retryable_embed: stats.failed_retryable_embed,
        failed_retryable_llm: stats.failed_retryable_llm,
        last_auto_requeue_at_unix_ms: stats.last_auto_requeue_at_unix_ms,
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
    let degraded_reasons = routes
        .iter()
        .flat_map(|route| route.degraded_reasons.iter().cloned())
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
    if endpoint_reachable && !degraded_reasons.is_empty() {
        warnings.push(format!(
            "REST endpoint reports degraded status: {}",
            degraded_reasons.join("; ")
        ));
        if degraded_reasons
            .iter()
            .any(|reason| reason.starts_with("schema_skew:") || reason.starts_with("stale_daemon:"))
        {
            recommendations.push(
                "Restart or upgrade the mempal daemon serving this REST endpoint, then rerun `mempal doctor rest`."
                    .to_string(),
            );
        }
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
    } else if !degraded_reasons.is_empty() {
        "degraded"
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

fn push_daemon_outage_queue_guidance(
    warnings: &mut Vec<String>,
    recommendations: &mut Vec<String>,
    availability: &DoctorAvailabilityReport,
) {
    if let Some(message) = availability.warning_message() {
        warnings.push(format!(
            "{} availability: {message}",
            availability.severity.as_str().to_ascii_uppercase()
        ));
        recommendations.push(match availability.severity {
            DoctorAvailabilitySeverity::High => daemon_outage_queue_recovery_guidance().to_string(),
            DoctorAvailabilitySeverity::Unavailable => {
                "Restore access to the mempal config and database, then rerun `mempal doctor`."
                    .to_string()
            }
            DoctorAvailabilitySeverity::Normal => return,
        });
    }
}

fn push_daemon_warnings(
    warnings: &mut Vec<String>,
    recommendations: &mut Vec<String>,
    daemon: &DoctorDaemonReport,
) {
    if daemon
        .process
        .as_ref()
        .is_some_and(|process| process.exe_deleted)
    {
        warnings.push("running daemon executable has been deleted or replaced on disk".to_string());
        recommendations.push(
            "Run `mempal daemon restart` after upgrading so the resident daemon uses the current binary."
                .to_string(),
        );
    }
    if daemon
        .embedder
        .as_ref()
        .is_some_and(|embedder| embedder.cache_loaded && embedder.backend == "model2vec")
    {
        recommendations.push(
            "For a lower-memory long-lived daemon, set `[daemon].embedder_mode = \"remote\"` with a local/LAN OpenAI-compatible embedding endpoint, or `small_local` for the smaller in-process model, then run `mempal reindex` if vector dimensions changed."
                .to_string(),
        );
    }
}

pub fn inspect_daemon(db_path: &Path) -> (DoctorDaemonReport, DoctorAvailabilityObservation<bool>) {
    let mempal_home = db_path.parent().unwrap_or_else(|| Path::new("."));
    let embedder = read_embedder_status(mempal_home).ok().flatten();
    let pidfile = read_daemon_pid_file(mempal_home);
    let binary =
        crate::daemon_singleton::current_binary_name().unwrap_or_else(|| "mempal".to_string());
    let daemons = crate::daemon_singleton::enumerate_daemon_processes(&binary, db_path);
    let matched_pid = pidfile
        .as_ref()
        .ok()
        .copied()
        .flatten()
        .filter(|pid| daemons.iter().any(|daemon| daemon.pid == *pid));
    let pid = matched_pid
        .or_else(|| daemons.first().map(|daemon| daemon.pid))
        .or_else(|| {
            pidfile.as_ref().ok().copied().flatten().or_else(|| {
                embedder
                    .as_ref()
                    .and_then(|status| i32::try_from(status.pid).ok())
            })
        });
    let process = pid.map(inspect_process_memory);
    let running = !daemons.is_empty();
    let observation = if running {
        DoctorAvailabilityObservation::Known(true)
    } else if pidfile.is_err()
        || process
            .as_ref()
            .is_some_and(|process| process.error.is_none())
    {
        DoctorAvailabilityObservation::Unavailable(
            DoctorAvailabilityUnavailableReason::DaemonIdentity,
        )
    } else {
        DoctorAvailabilityObservation::Known(false)
    };
    (
        DoctorDaemonReport {
            pid,
            running,
            embedder,
            process,
        },
        observation,
    )
}

fn read_daemon_pid_file(mempal_home: &Path) -> io::Result<Option<i32>> {
    let pid_path = mempal_home.join("daemon.pid");
    let content = match fs::read_to_string(&pid_path) {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let pid = content.trim().parse::<i32>().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid daemon pid in {}: {error}", pid_path.display()),
        )
    })?;
    Ok(Some(pid))
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
                    degraded_reasons: Vec::new(),
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
                    degraded_reasons: Vec::new(),
                });
                continue;
            }
        };
        match request.send().await {
            Ok(response) => {
                let status = response.status();
                let (available, error) = classify_rest_route_probe(method, path, status);
                let body = if *path == "/api/status" {
                    response.text().await.ok()
                } else {
                    None
                };
                let degraded_reasons = status_route_degraded_reasons(path, status, body.as_deref());
                reports.push(RestRouteReport {
                    method: (*method).to_string(),
                    path: (*path).to_string(),
                    available,
                    http_status: Some(status.as_u16()),
                    error,
                    degraded_reasons,
                });
            }
            Err(error) => reports.push(RestRouteReport {
                method: (*method).to_string(),
                path: (*path).to_string(),
                available: false,
                http_status: None,
                error: Some(error.to_string()),
                degraded_reasons: Vec::new(),
            }),
        }
    }
    reports
}

fn status_route_degraded_reasons(
    path: &str,
    status: reqwest::StatusCode,
    body: Option<&str>,
) -> Vec<String> {
    if path != "/api/status" || !status.is_success() {
        return Vec::new();
    }
    let Some(body) = body else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    value
        .get("status_warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(rest_degraded_reason_from_status_warning)
        .collect()
}

fn rest_degraded_reason_from_status_warning(warning: &str) -> String {
    let safe = crate::core::config::scrub_sensitive_text(warning);
    let lower = safe.to_ascii_lowercase();
    if lower.contains("schema") && lower.contains("newer than") {
        format!("schema_skew: {safe}")
    } else if lower.contains("daemon binary")
        || lower.contains("deleted or replaced")
        || lower.contains("stale daemon")
    {
        format!("stale_daemon: {safe}")
    } else {
        format!("status_warning: {safe}")
    }
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
        "/api/search" => format!(
            "{endpoint}{path}?q=mempal-doctor-rest&wing=hermes-user%2Fhermes-user%2Fdefault&include_raw_turns=false&top_k=0"
        ),
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
    fn daemon_outage_queue_availability_requires_daemon_down_and_large_queue() {
        for (daemon_running, pending_queue) in [
            (true, DAEMON_OUTAGE_PENDING_QUEUE_THRESHOLD),
            (false, DAEMON_OUTAGE_PENDING_QUEUE_THRESHOLD - 1),
        ] {
            let availability = daemon_outage_queue_availability(
                DoctorAvailabilityObservation::Known(daemon_running),
                DoctorAvailabilityObservation::Known(pending_queue),
            );

            assert_eq!(availability.severity, DoctorAvailabilitySeverity::Normal);
            assert_eq!(availability.signal, None);
        }
    }

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

    #[test]
    fn test_daemon_warnings_report_deleted_exe_and_low_memory_recommendation() {
        let report = DoctorDaemonReport {
            pid: Some(42),
            running: true,
            embedder: Some(DaemonEmbedderRuntimeStatus {
                pid: 42,
                process_identity: "test-process".to_string(),
                updated_at_unix_secs: 123,
                cache_loaded: true,
                mode: "configured".to_string(),
                backend: "model2vec".to_string(),
                model: Some("minishlab/potion-multilingual-128M".to_string()),
                dimensions: Some(1024),
                fallback: None,
                source: "test".to_string(),
            }),
            process: Some(ProcessMemoryReport {
                pid: 42,
                exe_deleted: true,
                exe_path: Some("/usr/local/bin/mempal (deleted)".to_string()),
                ..ProcessMemoryReport::default()
            }),
        };
        let mut warnings = Vec::new();
        let mut recommendations = Vec::new();

        push_daemon_warnings(&mut warnings, &mut recommendations, &report);

        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("deleted or replaced"))
        );
        assert!(
            recommendations
                .iter()
                .any(|recommendation| recommendation.contains("mempal daemon restart"))
        );
        assert!(
            recommendations
                .iter()
                .any(|recommendation| recommendation.contains("embedder_mode = \"remote\""))
        );
    }

    #[test]
    fn test_embedding_report_redacts_endpoint_url_paths() {
        let config = Config::parse(
            r#"
[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "http://127.0.0.1:18002/v1/private-token-path"
model = "Qwen/Qwen3-Embedding-8B"
"#,
        )
        .expect("parse config");

        let report =
            build_embedding_report(Some(&config), Path::new("/tmp/missing-palace.db"), None).report;

        assert_eq!(report.endpoints.len(), 1);
        assert_eq!(report.endpoints[0].base_url, "http://127.0.0.1:18002");
        assert!(!report.endpoints[0].base_url.contains("private-token-path"));
    }

    #[test]
    fn test_embedding_report_redacts_blocked_remote_endpoint_identity() {
        let config = Config::parse(
            r#"
[privacy.remote_calls]
fail_closed = true

[embed]
backend = "openai_compat"
base_url = "https://api.openai.com/v1/private-token-path"
api_model = "Qwen/Qwen3-Embedding-8B"

[embed.openai_compat]
api_key_env = "MEMPAL_SECRET_TOKEN_ENV"
"#,
        )
        .expect("parse config");

        let report =
            build_embedding_report(Some(&config), Path::new("/tmp/missing-palace.db"), None).report;
        let rendered = serde_json::to_string(&report).expect("serialize report");

        assert_eq!(report.endpoints.len(), 1);
        assert_eq!(
            report.endpoints[0].base_url,
            crate::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL
        );
        assert!(!rendered.contains("api.openai.com"), "{rendered}");
        assert!(!rendered.contains("private-token-path"), "{rendered}");
        assert!(!rendered.contains("MEMPAL_SECRET_TOKEN_ENV"), "{rendered}");
    }

    #[test]
    fn test_embedding_report_redacts_blocked_remote_endpoint_runtime_error() {
        crate::embed::global_embed_status().reset_for_tests();
        let config = Config::parse(
            r#"
[privacy.remote_calls]
fail_closed = true

[embed]
backend = "openai_compat"

[[embed.endpoints]]
id = "doctor-blocked-remote"
backend = "openai_compat"
base_url = "https://api.openai.com:9443/v1/private-embed-path"
model = "text-embedding-3-large"
"#,
        )
        .expect("parse config");
        crate::embed::global_embed_status().record_endpoint_cooldown(
            "doctor-blocked-remote",
            Duration::from_secs(60),
            &crate::embed::EmbedError::Runtime(
                "failed https://api.openai.com:9443/v1/private-embed-path?api_key=sk-secret-should-not-print MEMPAL_SECRET_TOKEN_ENV"
                    .to_string(),
            ),
        );

        let report =
            build_embedding_report(Some(&config), Path::new("/tmp/missing-palace.db"), None).report;
        let rendered = serde_json::to_string(&report).expect("serialize report");

        assert_eq!(report.endpoints.len(), 1);
        assert_eq!(
            report.endpoints[0].base_url,
            crate::core::remote_calls::BLOCKED_REMOTE_ENDPOINT_LABEL
        );
        assert!(report.endpoints[0].last_error.is_none());
        assert!(!rendered.contains("api.openai.com"), "{rendered}");
        assert!(!rendered.contains("private-embed-path"), "{rendered}");
        assert!(!rendered.contains("MEMPAL_SECRET_TOKEN_ENV"), "{rendered}");
        assert!(
            !rendered.contains("sk-secret-should-not-print"),
            "{rendered}"
        );
        crate::embed::global_embed_status().reset_for_tests();
    }

    #[test]
    fn test_embedding_report_surfaces_degraded_write_refused_state() {
        let config = Config::parse(
            r#"
[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "http://127.0.0.1:18002/v1/private-token-path"
model = "Qwen/Qwen3-Embedding-8B"

[embed.degradation]
degrade_after_n_failures = 10
block_writes_when_degraded = true
"#,
        )
        .expect("parse config");
        let daemon_status = serde_json::json!({
            "embed_status": {
                "degraded": true,
                "block_writes_when_degraded": true,
                "write_refused": true,
                "fail_count": 10,
                "last_error": r#"failed {"url":"http://127.0.0.1:18002/v1/private-token-path?api_key=sk-secret-should-not-print"}"#,
                "last_success_at_unix_ms": null
            }
        });

        let report = build_embedding_report(
            Some(&config),
            Path::new("/tmp/missing-palace.db"),
            Some(&daemon_status),
        )
        .report;
        let rendered = serde_json::to_string(&report).expect("serialize report");

        assert_eq!(report.runtime_status_source, "daemon_rest");
        assert!(report.runtime_status_available);
        assert!(report.degraded);
        assert!(report.block_writes_when_degraded);
        assert!(report.write_refused);
        assert_eq!(report.fail_count, 10);
        assert!(report.last_error.is_some());
        assert!(!rendered.contains("private-token-path"), "{rendered}");
        assert!(
            !rendered.contains("sk-secret-should-not-print"),
            "{rendered}"
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
