use thiserror::Error;

/// Maximum UTF-8 byte length accepted for one MCP or REST ingest content field.
pub const MAX_INGEST_REQUEST_BYTES: usize = 10 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
#[error("ingest content is {actual_bytes} bytes; limit is {limit_bytes} bytes")]
pub struct IngestRequestTooLarge {
    pub actual_bytes: usize,
    pub limit_bytes: usize,
}

/// Validate an ingest request before scrubbing, serialization, or queue admission.
pub fn validate_ingest_request_bytes(content: &str) -> Result<(), IngestRequestTooLarge> {
    let actual_bytes = content.len();
    if actual_bytes > MAX_INGEST_REQUEST_BYTES {
        return Err(IngestRequestTooLarge {
            actual_bytes,
            limit_bytes: MAX_INGEST_REQUEST_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_byte_limit_accepts_exact_limit_and_rejects_limit_plus_one() {
        let exact = "x".repeat(MAX_INGEST_REQUEST_BYTES);
        validate_ingest_request_bytes(&exact).expect("exact byte limit must be accepted");

        let oversized = format!("{exact}x");
        let error = validate_ingest_request_bytes(&oversized)
            .expect_err("limit plus one byte must be rejected");
        assert_eq!(error.actual_bytes, MAX_INGEST_REQUEST_BYTES + 1);
        assert_eq!(error.limit_bytes, MAX_INGEST_REQUEST_BYTES);
    }
}
