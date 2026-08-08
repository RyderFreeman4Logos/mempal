use std::fs;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeWriterHolderLiveness {
    Live,
    Dead,
    Unverifiable,
}

#[cfg(target_os = "linux")]
pub(super) fn runtime_boot_id() -> Option<String> {
    crate::core::process_identity::boot_id()
}

#[cfg(not(target_os = "linux"))]
pub(super) fn runtime_boot_id() -> Option<String> {
    None
}

fn runtime_writer_process_liveness(
    pid: u32,
    boot_id: Option<&str>,
    metadata_json: Option<&str>,
) -> RuntimeWriterHolderLiveness {
    if pid == 0 {
        return RuntimeWriterHolderLiveness::Dead;
    }

    let metadata = metadata_json.and_then(|value| serde_json::from_str::<Value>(value).ok());
    let expected_identity = metadata
        .as_ref()
        .and_then(|value| value.get("process_identity"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let expected_pid_namespace = metadata
        .as_ref()
        .and_then(|value| value.get("pid_namespace"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());

    #[cfg(target_os = "linux")]
    {
        if let (Some(expected_identity), Some(expected_pid_namespace)) =
            (expected_identity, expected_pid_namespace)
        {
            return match crate::core::process_identity::process_identity_liveness(
                pid,
                expected_identity,
                Some(expected_pid_namespace),
            ) {
                crate::core::process_identity::ProcessLiveness::Live => {
                    RuntimeWriterHolderLiveness::Live
                }
                crate::core::process_identity::ProcessLiveness::Dead => {
                    RuntimeWriterHolderLiveness::Dead
                }
                crate::core::process_identity::ProcessLiveness::Unverifiable => {
                    RuntimeWriterHolderLiveness::Unverifiable
                }
            };
        }

        let actual_boot_id = runtime_boot_id();
        if actual_boot_id
            .as_deref()
            .zip(boot_id)
            .is_some_and(|(actual, expected)| actual != expected)
        {
            return RuntimeWriterHolderLiveness::Dead;
        }

        match fs::metadata(format!("/proc/{pid}")) {
            Ok(_) if actual_boot_id.is_some() => RuntimeWriterHolderLiveness::Live,
            Ok(_) => RuntimeWriterHolderLiveness::Unverifiable,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                RuntimeWriterHolderLiveness::Dead
            }
            Err(_) => RuntimeWriterHolderLiveness::Unverifiable,
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        if pid != std::process::id() {
            return RuntimeWriterHolderLiveness::Unverifiable;
        }
        if expected_identity == Some(crate::core::process_identity::current_process_identity()) {
            RuntimeWriterHolderLiveness::Live
        } else {
            RuntimeWriterHolderLiveness::Dead
        }
    }
}

#[cfg(target_os = "linux")]
pub(super) fn runtime_writer_daemon_is_live_holder(
    owner: &str,
    pid: u32,
    boot_id: Option<&str>,
) -> bool {
    let Some(expected_boot_id) = boot_id else {
        return false;
    };
    runtime_boot_id().as_deref() == Some(expected_boot_id)
        && crate::core::process_identity::daemon_owner_matches_process(owner, pid)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn runtime_writer_daemon_is_live_holder(
    owner: &str,
    pid: u32,
    _boot_id: Option<&str>,
) -> bool {
    crate::core::process_identity::daemon_owner_matches_process(owner, pid)
}

pub(super) fn runtime_writer_lease_holder_is_live(
    owner: &str,
    pid: u32,
    boot_id: Option<&str>,
    mode: &str,
    metadata_json: Option<&str>,
) -> bool {
    if mode == "daemon" {
        runtime_writer_daemon_is_live_holder(owner, pid, boot_id)
    } else {
        matches!(
            runtime_writer_process_liveness(pid, boot_id, metadata_json),
            RuntimeWriterHolderLiveness::Live
        )
    }
}

pub(super) fn runtime_writer_lease_holder_should_retain(
    owner: &str,
    pid: u32,
    boot_id: Option<&str>,
    mode: &str,
    metadata_json: Option<&str>,
) -> bool {
    if mode == "daemon" {
        runtime_writer_daemon_is_live_holder(owner, pid, boot_id)
    } else {
        !matches!(
            runtime_writer_process_liveness(pid, boot_id, metadata_json),
            RuntimeWriterHolderLiveness::Dead
        )
    }
}

pub(super) fn runtime_writer_metadata_with_process_identity(metadata_json: Option<&str>) -> String {
    let mut metadata =
        match metadata_json.and_then(|value| serde_json::from_str::<Value>(value).ok()) {
            Some(Value::Object(metadata)) => metadata,
            Some(value) => {
                let mut metadata = serde_json::Map::new();
                metadata.insert("caller_metadata".to_string(), value);
                metadata
            }
            None => serde_json::Map::new(),
        };
    metadata.insert(
        "process_identity".to_string(),
        Value::String(crate::core::process_identity::current_process_identity().to_string()),
    );
    metadata.insert(
        "pid_namespace".to_string(),
        crate::core::process_identity::current_pid_namespace()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    Value::Object(metadata).to_string()
}
