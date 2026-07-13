//! Fact-check gating orchestration with generation-fenced audit writes.

use crate::core::config::AutoFactCheckConfig;
use crate::core::db::{Database, DbError};
use crate::core::types::RuntimeWriterLease;
use crate::factcheck;

use super::{
    FactCheckGateOutcome, fact_check_decision, fact_check_fail_open_outcome,
    format_fact_issue_warning,
};

pub fn evaluate_fact_check_gate(
    candidate_hash: &str,
    chunk_text: &str,
    db: &Database,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    project_id: Option<&str>,
    config: &AutoFactCheckConfig,
    new_confidence: f64,
) -> Result<Option<FactCheckGateOutcome>, DbError> {
    if !config.enabled {
        return Ok(None);
    }

    let now = match factcheck::resolve_now(None) {
        Ok(now) => now,
        Err(error) => {
            let outcome = fact_check_fail_open_outcome(error.to_string());
            record_audit(
                db,
                runtime_writer_lease,
                candidate_hash,
                chunk_text,
                project_id,
                &outcome,
            )?;
            return Ok(Some(outcome));
        }
    };

    let report = match factcheck::check_with_confidence(chunk_text, db, now, None, new_confidence) {
        Ok(report) => report,
        Err(error) => {
            let outcome = fact_check_fail_open_outcome(error.to_string());
            record_audit(
                db,
                runtime_writer_lease,
                candidate_hash,
                chunk_text,
                project_id,
                &outcome,
            )?;
            return Ok(Some(outcome));
        }
    };

    let warnings = report
        .issues
        .iter()
        .map(format_fact_issue_warning)
        .collect::<Vec<_>>();
    let decision = fact_check_decision(&report.issues, config);
    let outcome = FactCheckGateOutcome { decision, warnings };
    record_audit(
        db,
        runtime_writer_lease,
        candidate_hash,
        chunk_text,
        project_id,
        &outcome,
    )?;
    Ok(Some(outcome))
}

fn record_audit(
    db: &Database,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    candidate_hash: &str,
    chunk_text: &str,
    project_id: Option<&str>,
    outcome: &FactCheckGateOutcome,
) -> Result<(), DbError> {
    db.record_gating_audit_fenced(
        runtime_writer_lease,
        candidate_hash,
        &outcome.decision,
        project_id,
        Some(chunk_text),
        "record fact-check gating audit",
    )
}
