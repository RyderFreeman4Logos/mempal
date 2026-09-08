use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::Database;
use mempal::observability::{
    OperationTelemetryRecord, OperationTelemetrySource, record_operation_telemetry,
};

const MAX_ROWS: i64 = 50_000;

#[test]
fn telemetry_cap_prune_work_scales_with_excess_not_retained_rows() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let future = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_millis() as i64
        + 60_000;
    db.conn()
        .execute_batch(&format!(
            r#"
            WITH RECURSIVE sequence(value) AS (
                VALUES(0) UNION ALL SELECT value + 1 FROM sequence WHERE value < {MAX_ROWS} + 2
            )
            INSERT INTO operation_telemetry (
                id, started_at_unix_ms, duration_ms, source, operation, call_site, success
            )
            SELECT printf('telemetry-%d', value),
                   CASE WHEN value = 0 THEN 0 ELSE {future} + ((value - 1) / 2) END,
                   0, 'daemon', 'ingest', 'test', 1
            FROM sequence;
            "#
        ))
        .expect("seed stale and excess telemetry rows");

    let steps = Arc::new(AtomicUsize::new(0));
    let counted_steps = Arc::clone(&steps);
    db.conn().progress_handler(
        100,
        Some(move || {
            counted_steps.fetch_add(1, Ordering::Relaxed);
            false
        }),
    );
    record_operation_telemetry(
        &db,
        OperationTelemetryRecord::new(OperationTelemetrySource::Daemon, "ingest", "test"),
    )
    .expect("record and prune telemetry");
    db.conn().progress_handler(0, None::<fn() -> bool>);

    let executed_hundreds = steps.load(Ordering::Relaxed);
    assert!(
        executed_hundreds < 10,
        "pruning only excess rows executed at least {executed_hundreds}00 SQLite VM steps"
    );
    let retained: (i64, i64, String) = db
        .conn()
        .query_row(
            r#"
            SELECT COUNT(*), MIN(started_at_unix_ms),
                   (SELECT id FROM operation_telemetry
                    ORDER BY started_at_unix_ms, rowid LIMIT 1)
            FROM operation_telemetry
            "#,
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("inspect retained telemetry rows");
    assert_eq!(retained, (MAX_ROWS, future + 1, "telemetry-3".into()));
}
