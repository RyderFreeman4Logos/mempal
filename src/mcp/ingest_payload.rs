use std::path::PathBuf;

use rmcp::ErrorData;
use serde::{Deserialize, Serialize};

use super::server::ValidatedIngestMetadata;
use super::tools::{IngestControls, IngestRequest, IngestResponse};
use crate::core::db::Database;
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

pub(super) enum QueuedWriteOperation {
    Ingest(Box<QueuedIngestOperation>),
    Delete { drawer_id: String },
}

impl QueuedWriteOperation {
    pub(super) fn ingest(
        request: IngestRequest,
        controls: IngestControls,
        superseded_drawer_id: Option<String>,
    ) -> Self {
        Self::Ingest(Box::new(QueuedIngestOperation {
            request,
            controls,
            superseded_drawer_id,
        }))
    }
}

pub(super) fn decode_queued_ingest_operation(payload: &str) -> Option<QueuedWriteOperation> {
    if let Ok(prepared) = serde_json::from_str::<PreparedIngestOperation>(payload) {
        return Some(QueuedWriteOperation::ingest(
            prepared.request,
            prepared.controls,
            prepared.superseded_drawer_id,
        ));
    }

    if let Some(envelope) = crate::durable_ingest::DurableDeleteEnvelope::decode(payload) {
        return Some(QueuedWriteOperation::Delete {
            drawer_id: envelope.drawer_id,
        });
    }

    let envelope = crate::durable_ingest::DurableIngestEnvelope::decode(payload)?;
    let request = serde_json::from_value::<IngestRequest>(envelope.request).ok()?;
    let superseded_drawer_id = request.supersedes.clone();
    Some(QueuedWriteOperation::ingest(
        request,
        IngestControls::default(),
        superseded_drawer_id,
    ))
}

pub(super) async fn run_durable_delete(
    db_path: PathBuf,
    drawer_id: String,
) -> Result<IngestResponse, ErrorData> {
    super::stale_daemon::guard_write(&db_path)?;
    let result_id = drawer_id.clone();
    tokio::task::spawn_blocking(move || {
        let db = Database::open(&db_path)?;
        db.soft_delete_drawer(&drawer_id)
    })
    .await
    .map_err(|_| ErrorData::internal_error("durable delete database task failed", None))?
    .map_err(|error| {
        ErrorData::internal_error(
            format!("durable delete database operation failed: {error}"),
            None,
        )
    })?;
    Ok(IngestResponse {
        drawer_id: result_id,
        ..IngestResponse::default()
    })
}
