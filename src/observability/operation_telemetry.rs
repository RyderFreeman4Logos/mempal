use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;

use crate::core::db::Database;

static TELEMETRY_ID_COUNTER: AtomicU64 = AtomicU64::new(0);
const OPERATION_TELEMETRY_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const OPERATION_TELEMETRY_MAX_ROWS: i64 = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationTelemetrySource {
    Cli,
    Mcp,
    Rest,
    Daemon,
}

impl OperationTelemetrySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Mcp => "mcp",
            Self::Rest => "rest",
            Self::Daemon => "daemon",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OperationTelemetryIo {
    pub physical_read_bytes: u64,
    pub physical_write_bytes: u64,
    pub logical_read_bytes: u64,
    pub logical_write_bytes: u64,
    pub cancelled_write_bytes: u64,
}

impl OperationTelemetryIo {
    fn delta(start: Option<ProcIoSnapshot>, end: Option<ProcIoSnapshot>) -> Self {
        match (start, end) {
            (Some(start), Some(end)) => Self {
                physical_read_bytes: end.read_bytes.saturating_sub(start.read_bytes),
                physical_write_bytes: end.write_bytes.saturating_sub(start.write_bytes),
                logical_read_bytes: end.rchar.saturating_sub(start.rchar),
                logical_write_bytes: end.wchar.saturating_sub(start.wchar),
                cancelled_write_bytes: end
                    .cancelled_write_bytes
                    .saturating_sub(start.cancelled_write_bytes),
            },
            _ => Self::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OperationTelemetryRecord {
    source: OperationTelemetrySource,
    operation: String,
    call_site: String,
    started_at_unix_ms: i64,
    duration_ms: u64,
    success: bool,
    error_class: Option<String>,
    sqlite_error_class: Option<String>,
    rows_scanned: u64,
    rows_returned: u64,
    rows_changed: u64,
    lock_wait_count: u64,
    retry_count: u64,
    result_count: u64,
    io: OperationTelemetryIo,
    stage: Option<String>,
    search_mode: Option<String>,
    timed_out: Option<bool>,
    detached_task_continued: Option<bool>,
    detached_task_duration_ms: Option<u64>,
}

impl OperationTelemetryRecord {
    pub fn new(
        source: OperationTelemetrySource,
        operation: impl AsRef<str>,
        call_site: impl AsRef<str>,
    ) -> Self {
        Self {
            source,
            operation: bounded_label(operation.as_ref()),
            call_site: bounded_label(call_site.as_ref()),
            started_at_unix_ms: unix_ms_now(),
            duration_ms: 0,
            success: true,
            error_class: None,
            sqlite_error_class: None,
            rows_scanned: 0,
            rows_returned: 0,
            rows_changed: 0,
            lock_wait_count: 0,
            retry_count: 0,
            result_count: 0,
            io: OperationTelemetryIo::default(),
            stage: None,
            search_mode: None,
            timed_out: None,
            detached_task_continued: None,
            detached_task_duration_ms: None,
        }
    }

    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    pub fn with_success(mut self, success: bool) -> Self {
        self.success = success;
        if success {
            self.error_class = None;
            self.sqlite_error_class = None;
        }
        self
    }

    pub fn with_error(mut self, error: impl AsRef<str>) -> Self {
        let class = classify_error(error.as_ref());
        self.success = false;
        self.sqlite_error_class = sqlite_error_class(&class).map(str::to_string);
        self.error_class = Some(class);
        self
    }

    pub fn with_error_class(mut self, error_class: impl AsRef<str>) -> Self {
        let class = bounded_label(error_class.as_ref());
        self.success = false;
        self.sqlite_error_class = sqlite_error_class(&class).map(str::to_string);
        self.error_class = Some(class);
        self
    }

    pub fn with_result_count(mut self, result_count: u64) -> Self {
        self.result_count = result_count;
        self
    }

    pub fn with_rows_returned(mut self, rows_returned: u64) -> Self {
        self.rows_returned = rows_returned;
        self
    }

    pub fn with_rows_changed(mut self, rows_changed: u64) -> Self {
        self.rows_changed = rows_changed;
        self
    }

    pub fn with_rows_scanned(mut self, rows_scanned: u64) -> Self {
        self.rows_scanned = rows_scanned;
        self
    }

    pub fn with_lock_wait_count(mut self, lock_wait_count: u64) -> Self {
        self.lock_wait_count = lock_wait_count;
        self
    }

    pub fn with_retry_count(mut self, retry_count: u64) -> Self {
        self.retry_count = retry_count;
        self
    }

    pub fn with_io(mut self, io: OperationTelemetryIo) -> Self {
        self.io = io;
        self
    }

    pub fn with_stage(mut self, stage: impl AsRef<str>) -> Self {
        self.stage = Some(bounded_label(stage.as_ref()));
        self
    }

    pub fn with_search_mode(mut self, search_mode: impl AsRef<str>) -> Self {
        self.search_mode = Some(bounded_label(search_mode.as_ref()));
        self
    }

    pub fn with_timed_out(mut self, timed_out: bool) -> Self {
        self.timed_out = Some(timed_out);
        self
    }

    pub fn with_detached_task_info(mut self, continued: bool, duration_ms: Option<u64>) -> Self {
        self.detached_task_continued = Some(continued);
        self.detached_task_duration_ms = duration_ms;
        self
    }
}

#[derive(Debug, Serialize)]
struct OperationTelemetryMetadata<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    search_mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timed_out: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detached_task_continued: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detached_task_duration_ms: Option<u64>,
}

impl OperationTelemetryRecord {
    fn metadata_json(&self) -> Result<String> {
        serde_json::to_string(&OperationTelemetryMetadata {
            stage: self.stage.as_deref(),
            search_mode: self.search_mode.as_deref(),
            timed_out: self.timed_out,
            detached_task_continued: self.detached_task_continued,
            detached_task_duration_ms: self.detached_task_duration_ms,
        })
        .context("failed to serialize operation telemetry metadata")
    }
}

pub struct OperationTelemetrySpan {
    db_path: PathBuf,
    template: OperationTelemetryRecord,
    started_at: Instant,
    start_io: Option<ProcIoSnapshot>,
    finished: bool,
}

impl OperationTelemetrySpan {
    pub fn start(db_path: impl Into<PathBuf>, template: OperationTelemetryRecord) -> Self {
        Self {
            db_path: db_path.into(),
            template,
            started_at: Instant::now(),
            start_io: read_proc_io_snapshot(),
            finished: false,
        }
    }

    pub fn finish_success(mut self) {
        let record = self.finished_record(true, None);
        self.finished = true;
        crate::observability::record_io_burst_sample(
            crate::observability::classify_io_operation_path(&record.operation, &record.call_site),
            record.io.physical_read_bytes,
            record.io.logical_read_bytes,
            record.duration_ms,
        );
        let _ = record_operation_telemetry_path(&self.db_path, record);
    }

    pub fn finish_error(mut self, error: impl std::fmt::Display) {
        let record = self.finished_record(false, Some(error.to_string()));
        self.finished = true;
        crate::observability::record_io_burst_sample(
            crate::observability::classify_io_operation_path(&record.operation, &record.call_site),
            record.io.physical_read_bytes,
            record.io.logical_read_bytes,
            record.duration_ms,
        );
        let _ = record_operation_telemetry_path(&self.db_path, record);
    }

    pub fn finish_error_class(mut self, error_class: impl AsRef<str>) {
        let record = self
            .template
            .clone()
            .with_duration_ms(duration_ms(self.started_at.elapsed()))
            .with_io(OperationTelemetryIo::delta(
                self.start_io,
                read_proc_io_snapshot(),
            ))
            .with_error_class(error_class);
        self.finished = true;
        crate::observability::record_io_burst_sample(
            crate::observability::classify_io_operation_path(&record.operation, &record.call_site),
            record.io.physical_read_bytes,
            record.io.logical_read_bytes,
            record.duration_ms,
        );
        let _ = record_operation_telemetry_path(&self.db_path, record);
    }

    pub fn finish_result<T, E: std::fmt::Display>(mut self, result: &std::result::Result<T, E>) {
        let record = match result {
            Ok(_) => self.finished_record(true, None),
            Err(error) => self.finished_record(false, Some(error.to_string())),
        };
        self.finished = true;
        crate::observability::record_io_burst_sample(
            crate::observability::classify_io_operation_path(&record.operation, &record.call_site),
            record.io.physical_read_bytes,
            record.io.logical_read_bytes,
            record.duration_ms,
        );
        let _ = record_operation_telemetry_path(&self.db_path, record);
    }

    fn finished_record(&self, success: bool, error: Option<String>) -> OperationTelemetryRecord {
        let mut record = self
            .template
            .clone()
            .with_duration_ms(duration_ms(self.started_at.elapsed()))
            .with_io(OperationTelemetryIo::delta(
                self.start_io,
                read_proc_io_snapshot(),
            ));
        if success {
            record = record.with_success(true);
        } else if let Some(error) = error {
            record = record.with_error(error);
        } else {
            record = record.with_error_class("error");
        }
        record
    }
}

impl Drop for OperationTelemetrySpan {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let record = self.finished_record(false, Some("unfinished_span".to_string()));
        crate::observability::record_io_burst_sample(
            crate::observability::classify_io_operation_path(&record.operation, &record.call_site),
            record.io.physical_read_bytes,
            record.io.logical_read_bytes,
            record.duration_ms,
        );
        let _ = record_operation_telemetry_path(&self.db_path, record);
        self.finished = true;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct OperationTelemetrySummaryOptions {
    pub since_unix_ms: Option<i64>,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy)]
pub enum OperationTelemetryFormat {
    Plain,
    Json,
}

#[derive(Debug, Clone, Serialize)]
pub struct OperationTelemetrySummaryRow {
    pub source: String,
    pub operation: String,
    pub call_site: String,
    pub operation_count: u64,
    pub success_count: u64,
    pub error_count: u64,
    pub duration_ms_avg: u64,
    pub duration_ms_p50: u64,
    pub duration_ms_p95: u64,
    pub duration_ms_max: u64,
    pub physical_read_bytes_total: u64,
    pub physical_read_bytes_max: u64,
    pub physical_write_bytes_total: u64,
    pub physical_write_bytes_max: u64,
    pub logical_read_bytes_total: u64,
    pub logical_read_bytes_max: u64,
    pub logical_write_bytes_total: u64,
    pub logical_write_bytes_max: u64,
    pub cancelled_write_bytes_total: u64,
    pub lock_wait_count_total: u64,
    pub retry_count_total: u64,
    pub rows_scanned_total: u64,
    pub rows_returned_total: u64,
    pub rows_changed_total: u64,
    pub result_count_total: u64,
    pub last_error_class: Option<String>,
    pub last_sqlite_error_class: Option<String>,
}

#[derive(Debug)]
struct AggregateAccumulator {
    source: String,
    operation: String,
    call_site: String,
    operation_count: u64,
    success_count: u64,
    error_count: u64,
    durations: Vec<u64>,
    physical_read_bytes_total: u64,
    physical_read_bytes_max: u64,
    physical_write_bytes_total: u64,
    physical_write_bytes_max: u64,
    logical_read_bytes_total: u64,
    logical_read_bytes_max: u64,
    logical_write_bytes_total: u64,
    logical_write_bytes_max: u64,
    cancelled_write_bytes_total: u64,
    lock_wait_count_total: u64,
    retry_count_total: u64,
    rows_scanned_total: u64,
    rows_returned_total: u64,
    rows_changed_total: u64,
    result_count_total: u64,
    last_error_class: Option<String>,
    last_sqlite_error_class: Option<String>,
}

impl AggregateAccumulator {
    fn new(source: String, operation: String, call_site: String) -> Self {
        Self {
            source,
            operation,
            call_site,
            operation_count: 0,
            success_count: 0,
            error_count: 0,
            durations: Vec::new(),
            physical_read_bytes_total: 0,
            physical_read_bytes_max: 0,
            physical_write_bytes_total: 0,
            physical_write_bytes_max: 0,
            logical_read_bytes_total: 0,
            logical_read_bytes_max: 0,
            logical_write_bytes_total: 0,
            logical_write_bytes_max: 0,
            cancelled_write_bytes_total: 0,
            lock_wait_count_total: 0,
            retry_count_total: 0,
            rows_scanned_total: 0,
            rows_returned_total: 0,
            rows_changed_total: 0,
            result_count_total: 0,
            last_error_class: None,
            last_sqlite_error_class: None,
        }
    }

    fn push(&mut self, row: RawTelemetryRow) {
        self.operation_count = self.operation_count.saturating_add(1);
        if row.success {
            self.success_count = self.success_count.saturating_add(1);
        } else {
            self.error_count = self.error_count.saturating_add(1);
            if self.last_error_class.is_none() {
                self.last_error_class = row.error_class.clone();
            }
            if self.last_sqlite_error_class.is_none() {
                self.last_sqlite_error_class = row.sqlite_error_class.clone();
            }
        }
        self.durations.push(row.duration_ms);
        self.physical_read_bytes_total = self
            .physical_read_bytes_total
            .saturating_add(row.physical_read_bytes);
        self.physical_read_bytes_max = self.physical_read_bytes_max.max(row.physical_read_bytes);
        self.physical_write_bytes_total = self
            .physical_write_bytes_total
            .saturating_add(row.physical_write_bytes);
        self.physical_write_bytes_max = self.physical_write_bytes_max.max(row.physical_write_bytes);
        self.logical_read_bytes_total = self
            .logical_read_bytes_total
            .saturating_add(row.logical_read_bytes);
        self.logical_read_bytes_max = self.logical_read_bytes_max.max(row.logical_read_bytes);
        self.logical_write_bytes_total = self
            .logical_write_bytes_total
            .saturating_add(row.logical_write_bytes);
        self.logical_write_bytes_max = self.logical_write_bytes_max.max(row.logical_write_bytes);
        self.cancelled_write_bytes_total = self
            .cancelled_write_bytes_total
            .saturating_add(row.cancelled_write_bytes);
        self.lock_wait_count_total = self
            .lock_wait_count_total
            .saturating_add(row.lock_wait_count);
        self.retry_count_total = self.retry_count_total.saturating_add(row.retry_count);
        self.rows_scanned_total = self.rows_scanned_total.saturating_add(row.rows_scanned);
        self.rows_returned_total = self.rows_returned_total.saturating_add(row.rows_returned);
        self.rows_changed_total = self.rows_changed_total.saturating_add(row.rows_changed);
        self.result_count_total = self.result_count_total.saturating_add(row.result_count);
    }

    fn into_summary(mut self) -> OperationTelemetrySummaryRow {
        self.durations.sort_unstable();
        let duration_sum = self.durations.iter().copied().sum::<u64>();
        let duration_avg = duration_sum.checked_div(self.operation_count).unwrap_or(0);
        let duration_max = self.durations.last().copied().unwrap_or(0);
        OperationTelemetrySummaryRow {
            source: self.source,
            operation: self.operation,
            call_site: self.call_site,
            operation_count: self.operation_count,
            success_count: self.success_count,
            error_count: self.error_count,
            duration_ms_avg: duration_avg,
            duration_ms_p50: percentile(&self.durations, 50),
            duration_ms_p95: percentile(&self.durations, 95),
            duration_ms_max: duration_max,
            physical_read_bytes_total: self.physical_read_bytes_total,
            physical_read_bytes_max: self.physical_read_bytes_max,
            physical_write_bytes_total: self.physical_write_bytes_total,
            physical_write_bytes_max: self.physical_write_bytes_max,
            logical_read_bytes_total: self.logical_read_bytes_total,
            logical_read_bytes_max: self.logical_read_bytes_max,
            logical_write_bytes_total: self.logical_write_bytes_total,
            logical_write_bytes_max: self.logical_write_bytes_max,
            cancelled_write_bytes_total: self.cancelled_write_bytes_total,
            lock_wait_count_total: self.lock_wait_count_total,
            retry_count_total: self.retry_count_total,
            rows_scanned_total: self.rows_scanned_total,
            rows_returned_total: self.rows_returned_total,
            rows_changed_total: self.rows_changed_total,
            result_count_total: self.result_count_total,
            last_error_class: self.last_error_class,
            last_sqlite_error_class: self.last_sqlite_error_class,
        }
    }
}

struct RawTelemetryRow {
    source: String,
    operation: String,
    call_site: String,
    duration_ms: u64,
    success: bool,
    error_class: Option<String>,
    sqlite_error_class: Option<String>,
    rows_scanned: u64,
    rows_returned: u64,
    rows_changed: u64,
    lock_wait_count: u64,
    retry_count: u64,
    result_count: u64,
    physical_read_bytes: u64,
    physical_write_bytes: u64,
    logical_read_bytes: u64,
    logical_write_bytes: u64,
    cancelled_write_bytes: u64,
}

pub fn record_operation_telemetry(db: &Database, record: OperationTelemetryRecord) -> Result<()> {
    insert_operation_telemetry(db.conn(), record)
}

pub fn operation_telemetry_summary(
    db: &Database,
    options: OperationTelemetrySummaryOptions,
) -> Result<Vec<OperationTelemetrySummaryRow>> {
    if !operation_telemetry_table_exists(db.conn())? {
        return Ok(Vec::new());
    }

    let since = options.since_unix_ms.unwrap_or(0);
    let row_limit = i64::try_from(options.limit.max(1).saturating_mul(200)).unwrap_or(i64::MAX);
    let mut stmt = db
        .conn()
        .prepare(
            r#"
            SELECT
                source,
                operation,
                call_site,
                duration_ms,
                success,
                error_class,
                sqlite_error_class,
                rows_scanned,
                rows_returned,
                rows_changed,
                lock_wait_count,
                retry_count,
                result_count,
                physical_read_bytes,
                physical_write_bytes,
                logical_read_bytes,
                logical_write_bytes,
                cancelled_write_bytes
            FROM operation_telemetry
            WHERE started_at_unix_ms >= ?1
            ORDER BY started_at_unix_ms DESC
            LIMIT ?2
            "#,
        )
        .context("failed to prepare operation telemetry summary query")?;
    let rows = stmt
        .query_map(params![since, row_limit], |row| {
            Ok(RawTelemetryRow {
                source: row.get(0)?,
                operation: row.get(1)?,
                call_site: row.get(2)?,
                duration_ms: i64_to_u64(row.get::<_, i64>(3)?),
                success: row.get::<_, i64>(4)? != 0,
                error_class: row.get(5)?,
                sqlite_error_class: row.get(6)?,
                rows_scanned: i64_to_u64(row.get::<_, i64>(7)?),
                rows_returned: i64_to_u64(row.get::<_, i64>(8)?),
                rows_changed: i64_to_u64(row.get::<_, i64>(9)?),
                lock_wait_count: i64_to_u64(row.get::<_, i64>(10)?),
                retry_count: i64_to_u64(row.get::<_, i64>(11)?),
                result_count: i64_to_u64(row.get::<_, i64>(12)?),
                physical_read_bytes: i64_to_u64(row.get::<_, i64>(13)?),
                physical_write_bytes: i64_to_u64(row.get::<_, i64>(14)?),
                logical_read_bytes: i64_to_u64(row.get::<_, i64>(15)?),
                logical_write_bytes: i64_to_u64(row.get::<_, i64>(16)?),
                cancelled_write_bytes: i64_to_u64(row.get::<_, i64>(17)?),
            })
        })
        .context("failed to query operation telemetry rows")?;

    let mut groups: BTreeMap<(String, String, String), AggregateAccumulator> = BTreeMap::new();
    for row in rows {
        let row = row.context("failed to load operation telemetry row")?;
        groups
            .entry((
                row.source.clone(),
                row.operation.clone(),
                row.call_site.clone(),
            ))
            .or_insert_with(|| {
                AggregateAccumulator::new(
                    row.source.clone(),
                    row.operation.clone(),
                    row.call_site.clone(),
                )
            })
            .push(row);
    }

    let mut summaries = groups
        .into_values()
        .map(AggregateAccumulator::into_summary)
        .collect::<Vec<_>>();
    summaries.sort_by(|a, b| {
        b.physical_read_bytes_total
            .cmp(&a.physical_read_bytes_total)
            .then_with(|| {
                b.physical_write_bytes_total
                    .cmp(&a.physical_write_bytes_total)
            })
            .then_with(|| b.duration_ms_max.cmp(&a.duration_ms_max))
            .then_with(|| b.operation_count.cmp(&a.operation_count))
    });
    summaries.truncate(options.limit);
    Ok(summaries)
}

fn operation_telemetry_table_exists(conn: &Connection) -> Result<bool> {
    let exists = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='operation_telemetry')",
            [],
            |row| row.get::<_, i64>(0),
        )
        .context("failed to check operation telemetry table")?;
    Ok(exists == 1)
}

pub fn render_operation_telemetry_summary(
    rows: &[OperationTelemetrySummaryRow],
    format: OperationTelemetryFormat,
) -> Result<String> {
    match format {
        OperationTelemetryFormat::Json => {
            serde_json::to_string_pretty(rows).context("failed to serialize operation telemetry")
        }
        OperationTelemetryFormat::Plain => Ok(render_plain(rows)),
    }
}

fn record_operation_telemetry_path(path: &Path, record: OperationTelemetryRecord) -> Result<()> {
    let conn = Connection::open(path).context("failed to open telemetry database")?;
    conn.busy_timeout(Duration::from_millis(10))
        .context("failed to set telemetry busy timeout")?;
    insert_operation_telemetry(&conn, record)
}

fn insert_operation_telemetry(conn: &Connection, record: OperationTelemetryRecord) -> Result<()> {
    let id = next_telemetry_id(&record);
    let metadata_json = record.metadata_json()?;
    conn.execute(
        r#"
        INSERT INTO operation_telemetry (
            id,
            started_at_unix_ms,
            duration_ms,
            source,
            operation,
            call_site,
            success,
            error_class,
            sqlite_error_class,
            rows_scanned,
            rows_returned,
            rows_changed,
            lock_wait_count,
            retry_count,
            result_count,
            physical_read_bytes,
            physical_write_bytes,
            logical_read_bytes,
            logical_write_bytes,
            cancelled_write_bytes,
            metadata_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
        "#,
        params![
            id,
            record.started_at_unix_ms,
            u64_to_i64(record.duration_ms),
            record.source.as_str(),
            record.operation,
            record.call_site,
            if record.success { 1_i64 } else { 0_i64 },
            record.error_class,
            record.sqlite_error_class,
            u64_to_i64(record.rows_scanned),
            u64_to_i64(record.rows_returned),
            u64_to_i64(record.rows_changed),
            u64_to_i64(record.lock_wait_count),
            u64_to_i64(record.retry_count),
            u64_to_i64(record.result_count),
            u64_to_i64(record.io.physical_read_bytes),
            u64_to_i64(record.io.physical_write_bytes),
            u64_to_i64(record.io.logical_read_bytes),
            u64_to_i64(record.io.logical_write_bytes),
            u64_to_i64(record.io.cancelled_write_bytes),
            metadata_json,
        ],
    )
    .context("failed to insert operation telemetry")?;
    prune_operation_telemetry(conn, record.started_at_unix_ms)
        .context("failed to prune operation telemetry")?;
    Ok(())
}

fn prune_operation_telemetry(conn: &Connection, now_unix_ms: i64) -> Result<()> {
    let cutoff = now_unix_ms.saturating_sub(OPERATION_TELEMETRY_RETENTION_MS);
    conn.execute(
        "DELETE FROM operation_telemetry WHERE started_at_unix_ms < ?1",
        params![cutoff],
    )
    .context("failed to prune old operation telemetry rows")?;
    conn.execute(
        r#"
        DELETE FROM operation_telemetry
        WHERE rowid IN (
            SELECT rowid
            FROM operation_telemetry
            ORDER BY started_at_unix_ms DESC, rowid DESC
            LIMIT -1 OFFSET ?1
        )
        "#,
        params![OPERATION_TELEMETRY_MAX_ROWS],
    )
    .context("failed to cap operation telemetry rows")?;
    Ok(())
}

fn render_plain(rows: &[OperationTelemetrySummaryRow]) -> String {
    let mut output = String::from("Operation telemetry:\n");
    if rows.is_empty() {
        output.push_str("  no recent operation telemetry\n");
        return output;
    }
    output.push_str(
        "  source operation call_site count ok err avg_ms p95_ms max_ms phys_read phys_write logical_read logical_write retries locks last_error\n",
    );
    for row in rows {
        output.push_str(&format!(
            "  {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
            row.source,
            row.operation,
            row.call_site,
            row.operation_count,
            row.success_count,
            row.error_count,
            row.duration_ms_avg,
            row.duration_ms_p95,
            row.duration_ms_max,
            row.physical_read_bytes_total,
            row.physical_write_bytes_total,
            row.logical_read_bytes_total,
            row.logical_write_bytes_total,
            row.retry_count_total,
            row.lock_wait_count_total,
            row.last_error_class.as_deref().unwrap_or("none")
        ));
    }
    output
}

fn next_telemetry_id(record: &OperationTelemetryRecord) -> String {
    let counter = TELEMETRY_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    next_telemetry_id_with_process(record, std::process::id(), counter)
}

fn next_telemetry_id_with_process(
    record: &OperationTelemetryRecord,
    process_id: u32,
    counter: u64,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(record.source.as_str().as_bytes());
    hasher.update(record.operation.as_bytes());
    hasher.update(record.call_site.as_bytes());
    hasher.update(&record.started_at_unix_ms.to_le_bytes());
    hasher.update(&process_id.to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    format!("optelemetry_{}", &hasher.finalize().to_hex()[..24])
}

fn bounded_label(raw: &str) -> String {
    let trimmed = raw.trim();
    if is_bounded_label(trimmed) {
        return trimmed.to_string();
    }
    let digest = blake3::hash(trimmed.as_bytes()).to_hex();
    format!("untrusted_{}", &digest[..12])
}

fn is_bounded_label(value: &str) -> bool {
    if value.is_empty() || value.len() > 80 {
        return false;
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':' | '/' | ' '))
    {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    ![
        "select ",
        "insert ",
        " insert ",
        "update ",
        " update ",
        "delete ",
        " delete ",
        "where ",
        " where ",
        "authorization",
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "?",
        "=",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn classify_error(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if lower.contains("database is locked") || lower.contains(" locked") {
        "locked".to_string()
    } else if lower.contains("busy") {
        "busy".to_string()
    } else if lower.contains("protocol") {
        "protocol".to_string()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timeout".to_string()
    } else if lower.contains("cancel") {
        "cancelled".to_string()
    } else {
        "error".to_string()
    }
}

fn sqlite_error_class(class: &str) -> Option<&'static str> {
    match class {
        "busy" => Some("busy"),
        "locked" => Some("locked"),
        "protocol" => Some("protocol"),
        _ => None,
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = sorted.len().saturating_mul(percentile).saturating_add(99) / 100;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

#[derive(Debug, Clone, Copy)]
struct ProcIoSnapshot {
    rchar: u64,
    wchar: u64,
    read_bytes: u64,
    write_bytes: u64,
    cancelled_write_bytes: u64,
}

#[cfg(target_os = "linux")]
fn read_proc_io_snapshot() -> Option<ProcIoSnapshot> {
    let raw = fs::read_to_string("/proc/self/io").ok()?;
    let mut snapshot = ProcIoSnapshot {
        rchar: 0,
        wchar: 0,
        read_bytes: 0,
        write_bytes: 0,
        cancelled_write_bytes: 0,
    };
    for line in raw.lines() {
        let (key, value) = line.split_once(':')?;
        let parsed = value.trim().parse::<u64>().ok()?;
        match key {
            "rchar" => snapshot.rchar = parsed,
            "wchar" => snapshot.wchar = parsed,
            "read_bytes" => snapshot.read_bytes = parsed,
            "write_bytes" => snapshot.write_bytes = parsed,
            "cancelled_write_bytes" => snapshot.cancelled_write_bytes = parsed,
            _ => {}
        }
    }
    Some(snapshot)
}

#[cfg(not(target_os = "linux"))]
fn read_proc_io_snapshot() -> Option<ProcIoSnapshot> {
    None
}

fn unix_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn u64_to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn i64_to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::db::Database;

    #[test]
    fn telemetry_id_includes_process_identity() {
        let mut record =
            OperationTelemetryRecord::new(OperationTelemetrySource::Cli, "ingest", "main");
        record.started_at_unix_ms = 42;

        let first = next_telemetry_id_with_process(&record, 100, 1);
        let second = next_telemetry_id_with_process(&record, 101, 1);

        assert_ne!(
            first, second,
            "same-millisecond same-counter events from separate processes must not collide"
        );
    }

    #[test]
    fn dropped_span_records_unfinished_error_not_success() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");

        drop(OperationTelemetrySpan::start(
            db_path,
            OperationTelemetryRecord::new(OperationTelemetrySource::Mcp, "mempal_ingest", "test"),
        ));

        let rows = operation_telemetry_summary(
            &db,
            OperationTelemetrySummaryOptions {
                since_unix_ms: None,
                limit: 10,
            },
        )
        .expect("summarize telemetry");
        let row = rows
            .iter()
            .find(|row| row.source == "mcp" && row.operation == "mempal_ingest")
            .expect("span telemetry row");

        assert_eq!(row.success_count, 0);
        assert_eq!(row.error_count, 1);
        assert_eq!(row.last_error_class.as_deref(), Some("error"));
    }
}
