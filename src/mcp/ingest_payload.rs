use serde::{Deserialize, Serialize};

use super::server::ValidatedIngestMetadata;
use super::tools::{IngestControls, IngestRequest};
use crate::core::types::SourceType;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct PreparedIngestOperation {
    pub(super) request: IngestRequest,
    #[serde(default)]
    pub(super) controls: IngestControls,
    pub(super) project_id: Option<String>,
    pub(super) scrubbed_content: String,
    pub(super) source_type: SourceType,
    pub(super) confidence: f64,
    pub(super) metadata: ValidatedIngestMetadata,
    pub(super) superseded_drawer_id: Option<String>,
    pub(super) raw_turn: bool,
    pub(super) drawer_importance: i32,
}

pub(super) struct QueuedIngestOperation {
    pub(super) request: IngestRequest,
    pub(super) controls: IngestControls,
    pub(super) superseded_drawer_id: Option<String>,
}

pub(super) fn decode_queued_ingest_operation(payload: &str) -> Option<QueuedIngestOperation> {
    if let Ok(prepared) = serde_json::from_str::<PreparedIngestOperation>(payload) {
        return Some(QueuedIngestOperation {
            request: prepared.request,
            controls: prepared.controls,
            superseded_drawer_id: prepared.superseded_drawer_id,
        });
    }

    let envelope = crate::durable_ingest::DurableIngestEnvelope::decode(payload)?;
    let request = serde_json::from_value::<IngestRequest>(envelope.request).ok()?;
    Some(QueuedIngestOperation {
        superseded_drawer_id: request.supersedes.clone(),
        request,
        controls: IngestControls::default(),
    })
}
