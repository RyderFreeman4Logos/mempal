//! Runtime writer-lease transfer object.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeWriterLease {
    pub name: String,
    pub owner: String,
    /// Monotonic fencing token for this lease name.
    pub generation: u64,
    pub pid: u32,
    pub boot_id: Option<String>,
    pub session_id: String,
    pub acquired_at: String,
    pub expires_at: String,
    pub heartbeat_at: String,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_json: Option<String>,
    pub remaining_secs: i64,
}
