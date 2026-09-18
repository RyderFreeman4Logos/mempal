use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::ErrorData;

use crate::durable_ingest::DurableAdmissionError;

use super::IngestAdmissionError;

pub(super) fn mcp_ingest_idempotency_key(
    payload: &str,
    caller_key: Option<&str>,
) -> std::result::Result<String, IngestAdmissionError> {
    if let Some(key) = caller_key {
        crate::durable_ingest::validate_idempotency_key(key).map_err(|_| {
            IngestAdmissionError::Mcp(ErrorData::invalid_params(
                DurableAdmissionError::InvalidIdempotencyKey.to_string(),
                None,
            ))
        })?;
        return Ok(key.to_string());
    }
    let now_ns = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => 0,
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mempal mcp ingest admission v1");
    hasher.update(&[0]);
    hasher.update(&now_ns.to_le_bytes());
    hasher.update(&[0]);
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&[0]);
    hasher.update(payload.as_bytes());
    Ok(format!("mcp-ingest-{}", hasher.finalize().to_hex()))
}
