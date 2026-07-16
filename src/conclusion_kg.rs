use std::path::Path;

use crate::core::config::Config;
use crate::core::db::{Database, DbError};
use crate::core::types::RuntimeWriterLease;
use crate::intelligence::IntelligenceRouter;

const SESSION_CONCLUSION_SOURCE: &str = "hermes-session-conclusion";

pub(crate) fn is_session_conclusion(source: Option<&str>) -> bool {
    source == Some(SESSION_CONCLUSION_SOURCE)
}

pub(crate) async fn populate_from_conclusion(
    db_path: &Path,
    config: &Config,
    content: &str,
    source_drawer: &str,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
) -> Result<usize, DbError> {
    let router = IntelligenceRouter::from_config(config);
    let mut triples = router.extract_kg_triples(content).await;
    if triples.is_empty() {
        return Ok(0);
    }

    for triple in &mut triples {
        triple.source_drawer = Some(source_drawer.to_string());
    }
    let db = Database::open(db_path)?;
    db.with_runtime_writer_lease_write(
        runtime_writer_lease,
        "insert session conclusion KG triples",
        || {
            for triple in &triples {
                db.insert_triple(triple)?;
            }
            Ok::<(), DbError>(())
        },
    )?;
    let triple_count = triples.len();
    tracing::info!(
        source_drawer,
        triple_count,
        "populated conclusion KG triples"
    );
    Ok(triple_count)
}
