use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Serialize;
use tempfile::tempdir;

use crate::core::{
    config::Config,
    db::Database,
    project::ProjectSearchScope,
    remote_calls::build_remote_call_report,
    types::{BootstrapEvidenceArgs, Drawer, SourceType},
};
use crate::ingest::normalize::CURRENT_NORMALIZE_VERSION;

const BENCH_WING: &str = "benchmark_matrix";
const BUILTIN_DATASET_ID: &str = "builtin_recall_citation_v1";
const BUILTIN_DATASET_VERSION: &str = "2026-06-22";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum BenchmarkMatrixModeArg {
    NoLlm,
    LocalLlm,
    CloudLlm,
    All,
}

impl BenchmarkMatrixModeArg {
    fn modes(self) -> Vec<BenchmarkMatrixMode> {
        match self {
            Self::NoLlm => vec![BenchmarkMatrixMode::NoLlm],
            Self::LocalLlm => vec![BenchmarkMatrixMode::LocalLlm],
            Self::CloudLlm => vec![BenchmarkMatrixMode::CloudLlm],
            Self::All => vec![
                BenchmarkMatrixMode::NoLlm,
                BenchmarkMatrixMode::LocalLlm,
                BenchmarkMatrixMode::CloudLlm,
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkMatrixMode {
    NoLlm,
    LocalLlm,
    CloudLlm,
}

impl BenchmarkMatrixMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoLlm => "no_llm",
            Self::LocalLlm => "local_llm",
            Self::CloudLlm => "cloud_llm",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "snake_case")]
#[clap(rename_all = "kebab-case")]
pub enum BenchmarkMatrixDataset {
    Builtin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
pub enum BenchmarkMatrixFormat {
    Plain,
    Json,
}

#[derive(Debug, Clone)]
pub struct BenchmarkMatrixArgs {
    pub dataset: BenchmarkMatrixDataset,
    pub mode: BenchmarkMatrixModeArg,
    pub top_k: usize,
    pub format: BenchmarkMatrixFormat,
    pub out: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BenchmarkMatrixReport {
    pub schema_version: &'static str,
    pub dataset: DatasetSummary,
    pub reproducibility: ReproducibilitySummary,
    pub runs: Vec<BenchmarkMatrixRunReport>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DatasetSummary {
    pub id: &'static str,
    pub version: &'static str,
    pub source: &'static str,
    pub records: usize,
    pub queries: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReproducibilitySummary {
    pub deterministic: bool,
    pub fixture_seed: &'static str,
    pub provider_calls_enabled: bool,
    pub notes: Vec<&'static str>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BenchmarkMatrixRunReport {
    pub mode: BenchmarkMatrixMode,
    pub status: &'static str,
    pub retrieval_engine: &'static str,
    pub provider_execution: &'static str,
    pub top_k: usize,
    pub question_count: usize,
    pub recall: RecallMetrics,
    pub citation: CitationMetrics,
    pub leakage: LeakageMetrics,
    pub stale_decision: StaleDecisionMetrics,
    pub latency: LatencySummary,
    pub resources: ResourceUsageSummary,
    pub remote_calls: RemoteCallMetrics,
    pub remote_call_config: Vec<RemoteCallConfigSummary>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RecallMetrics {
    pub k: usize,
    pub evaluated_queries: usize,
    pub hit_queries: usize,
    pub recall_at_k: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct CitationMetrics {
    pub evaluated_queries: usize,
    pub correct_queries: usize,
    pub correctness_at_k: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LeakageMetrics {
    pub evaluated_queries: usize,
    pub leaked_queries: usize,
    pub leakage_rate_at_k: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StaleDecisionMetrics {
    pub evaluated_queries: usize,
    pub false_positive_queries: usize,
    pub false_positive_rate_at_k: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LatencySummary {
    pub unit: &'static str,
    pub min: u64,
    pub p50: u64,
    pub p95: u64,
    pub max: u64,
    pub mean: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ResourceUsageSummary {
    pub rss_bytes: Option<u64>,
    pub scratch_sqlite_bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RemoteCallMetrics {
    pub embedding: u64,
    pub llm: u64,
    pub rerank: u64,
    pub total: u64,
    pub estimated_cost_usd: f64,
    pub recorded_cost_usd: f64,
    pub currency: &'static str,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RemoteCallConfigSummary {
    pub service: &'static str,
    pub status: &'static str,
    pub policy: &'static str,
    pub endpoint_configured: bool,
}

#[derive(Debug, Clone)]
struct Fixture {
    records: Vec<FixtureRecord>,
    queries: Vec<FixtureQuery>,
}

#[derive(Debug, Clone)]
struct FixtureRecord {
    drawer_id: &'static str,
    content: &'static str,
    project_id: &'static str,
    source_file: &'static str,
    added_at: &'static str,
    valid_until: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct FixtureQuery {
    query: &'static str,
    project_id: &'static str,
    expected_drawer_ids: &'static [&'static str],
    expected_source_files: &'static [&'static str],
    stale_drawer_ids: &'static [&'static str],
    leakage_check: bool,
}

#[derive(Debug, Clone)]
struct QueryOutcome {
    drawer_ids: Vec<String>,
    source_files: Vec<String>,
    project_ids: Vec<Option<String>>,
    latency_micros: u64,
}

pub fn default_matrix_top_k() -> usize {
    5
}

pub fn run_benchmark_matrix_command(config: &Config, args: BenchmarkMatrixArgs) -> Result<()> {
    let format = args.format;
    let out = args.out.clone();
    let report = run_benchmark_matrix(config, args)?;

    match format {
        BenchmarkMatrixFormat::Plain => print_plain_report(&report),
        BenchmarkMatrixFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&report)
                    .context("failed to serialize benchmark matrix report")?
            );
        }
    }

    if let Some(path) = out.as_deref() {
        write_report(path, &report)?;
        eprintln!("benchmark_report: {}", path.display());
    }

    Ok(())
}

pub fn run_benchmark_matrix(
    config: &Config,
    args: BenchmarkMatrixArgs,
) -> Result<BenchmarkMatrixReport> {
    if args.top_k == 0 {
        bail!("--top-k must be greater than 0");
    }

    let fixture = load_fixture(args.dataset);
    let runs = args
        .mode
        .modes()
        .into_iter()
        .map(|mode| run_mode(config, &fixture, mode, args.top_k))
        .collect::<Result<Vec<_>>>()?;

    Ok(BenchmarkMatrixReport {
        schema_version: "mempal.benchmark_matrix.v1",
        dataset: DatasetSummary {
            id: BUILTIN_DATASET_ID,
            version: BUILTIN_DATASET_VERSION,
            source: "built_in",
            records: fixture.records.len(),
            queries: fixture.queries.len(),
        },
        reproducibility: ReproducibilitySummary {
            deterministic: true,
            fixture_seed: "builtin-fixed",
            provider_calls_enabled: false,
            notes: vec![
                "default harness uses SQLite FTS/BM25 only",
                "model-backed modes are explicit report rows and do not call providers",
            ],
        },
        runs,
    })
}

fn run_mode(
    config: &Config,
    fixture: &Fixture,
    mode: BenchmarkMatrixMode,
    top_k: usize,
) -> Result<BenchmarkMatrixRunReport> {
    let scratch = tempdir().context("failed to create benchmark scratch directory")?;
    let db_path = scratch.path().join("matrix.db");
    let db = Database::open(&db_path)
        .with_context(|| format!("failed to open benchmark database {}", db_path.display()))?;
    seed_fixture(&db, fixture)?;

    let mut outcomes = Vec::with_capacity(fixture.queries.len());
    for query in &fixture.queries {
        outcomes.push(run_query(&db, query, top_k)?);
    }

    let latency_values = outcomes
        .iter()
        .map(|outcome| outcome.latency_micros)
        .collect::<Vec<_>>();
    let resources = ResourceUsageSummary {
        rss_bytes: current_rss_bytes(),
        scratch_sqlite_bytes: sqlite_artifact_bytes(&db_path),
    };
    let metrics = summarize_outcomes(fixture, &outcomes, top_k);

    Ok(BenchmarkMatrixRunReport {
        mode,
        status: "completed",
        retrieval_engine: "sqlite_fts_bm25",
        provider_execution: "disabled_deterministic_fixture",
        top_k,
        question_count: fixture.queries.len(),
        recall: metrics.0,
        citation: metrics.1,
        leakage: metrics.2,
        stale_decision: metrics.3,
        latency: summarize_latency(&latency_values),
        resources,
        remote_calls: RemoteCallMetrics {
            embedding: 0,
            llm: 0,
            rerank: 0,
            total: 0,
            estimated_cost_usd: 0.0,
            recorded_cost_usd: 0.0,
            currency: "USD",
        },
        remote_call_config: remote_call_config_summary(config),
    })
}

fn seed_fixture(db: &Database, fixture: &Fixture) -> Result<()> {
    for record in &fixture.records {
        let drawer = Drawer {
            normalize_version: CURRENT_NORMALIZE_VERSION,
            ..Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
                id: record.drawer_id.to_string(),
                content: record.content.to_string(),
                wing: BENCH_WING.to_string(),
                room: Some("recall".to_string()),
                source_file: Some(record.source_file.to_string()),
                source_type: SourceType::AgentObservation,
                added_at: record.added_at.to_string(),
                chunk_index: Some(0),
                importance: 3,
            })
        };
        db.insert_drawer_with_project_validity(
            &drawer,
            Some(record.project_id),
            None,
            None,
            record.valid_until,
        )
        .with_context(|| format!("failed to seed benchmark drawer {}", record.drawer_id))?;
    }
    Ok(())
}

fn run_query(db: &Database, query: &FixtureQuery, top_k: usize) -> Result<QueryOutcome> {
    let scope =
        ProjectSearchScope::from_request(Some(query.project_id.to_string()), false, false, true);
    let started = Instant::now();
    let hits = db
        .search_fts(
            query.query,
            Some(BENCH_WING),
            Some("recall"),
            scope.mode_param(),
            scope.project_id.as_deref(),
            top_k,
        )
        .with_context(|| {
            format!(
                "benchmark FTS query failed for project {}",
                query.project_id
            )
        })?;
    let latency_micros = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);

    let mut drawer_ids = Vec::with_capacity(hits.len());
    let mut source_files = Vec::with_capacity(hits.len());
    let mut project_ids = Vec::with_capacity(hits.len());
    for (drawer_id, _) in hits {
        let details = db
            .get_drawer_details(&drawer_id)
            .with_context(|| format!("failed to hydrate benchmark drawer {drawer_id}"))?
            .with_context(|| format!("benchmark drawer missing after search: {drawer_id}"))?;
        drawer_ids.push(drawer_id);
        source_files.push(details.drawer.source_file.unwrap_or_default());
        project_ids.push(details.project_id);
    }

    Ok(QueryOutcome {
        drawer_ids,
        source_files,
        project_ids,
        latency_micros,
    })
}

fn summarize_outcomes(
    fixture: &Fixture,
    outcomes: &[QueryOutcome],
    top_k: usize,
) -> (
    RecallMetrics,
    CitationMetrics,
    LeakageMetrics,
    StaleDecisionMetrics,
) {
    let mut recall_evaluated = 0usize;
    let mut recall_hits = 0usize;
    let mut citation_evaluated = 0usize;
    let mut citation_correct = 0usize;
    let mut leakage_evaluated = 0usize;
    let mut leaked = 0usize;
    let mut stale_evaluated = 0usize;
    let mut stale_false_positive = 0usize;

    for (query, outcome) in fixture.queries.iter().zip(outcomes) {
        if !query.expected_drawer_ids.is_empty() {
            recall_evaluated += 1;
            if contains_any(&outcome.drawer_ids, query.expected_drawer_ids) {
                recall_hits += 1;
            }
        }

        if !query.expected_source_files.is_empty() {
            citation_evaluated += 1;
            if citation_matches(outcome, query) {
                citation_correct += 1;
            }
        }

        if query.leakage_check {
            leakage_evaluated += 1;
            if outcome
                .project_ids
                .iter()
                .any(|project_id| project_id.as_deref() != Some(query.project_id))
            {
                leaked += 1;
            }
        }

        if !query.stale_drawer_ids.is_empty() {
            stale_evaluated += 1;
            if contains_any(&outcome.drawer_ids, query.stale_drawer_ids) {
                stale_false_positive += 1;
            }
        }
    }

    (
        RecallMetrics {
            k: top_k,
            evaluated_queries: recall_evaluated,
            hit_queries: recall_hits,
            recall_at_k: rate(recall_hits, recall_evaluated),
        },
        CitationMetrics {
            evaluated_queries: citation_evaluated,
            correct_queries: citation_correct,
            correctness_at_k: rate(citation_correct, citation_evaluated),
        },
        LeakageMetrics {
            evaluated_queries: leakage_evaluated,
            leaked_queries: leaked,
            leakage_rate_at_k: rate(leaked, leakage_evaluated),
        },
        StaleDecisionMetrics {
            evaluated_queries: stale_evaluated,
            false_positive_queries: stale_false_positive,
            false_positive_rate_at_k: rate(stale_false_positive, stale_evaluated),
        },
    )
}

fn contains_any(actual: &[String], expected: &[&str]) -> bool {
    let actual = actual.iter().map(String::as_str).collect::<BTreeSet<_>>();
    expected.iter().any(|id| actual.contains(id))
}

fn citation_matches(outcome: &QueryOutcome, query: &FixtureQuery) -> bool {
    outcome
        .drawer_ids
        .iter()
        .zip(outcome.source_files.iter())
        .any(|(drawer_id, source_file)| {
            query
                .expected_drawer_ids
                .iter()
                .any(|expected_id| drawer_id == expected_id)
                && query
                    .expected_source_files
                    .iter()
                    .any(|expected_source| source_file == expected_source)
        })
}

fn rate(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        return 0.0;
    }
    numerator as f64 / denominator as f64
}

fn summarize_latency(values: &[u64]) -> LatencySummary {
    if values.is_empty() {
        return LatencySummary {
            unit: "microseconds",
            min: 0,
            p50: 0,
            p95: 0,
            max: 0,
            mean: 0.0,
        };
    }

    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let sum = sorted.iter().sum::<u64>();
    LatencySummary {
        unit: "microseconds",
        min: sorted[0],
        p50: percentile(&sorted, 50),
        p95: percentile(&sorted, 95),
        max: sorted[sorted.len() - 1],
        mean: sum as f64 / sorted.len() as f64,
    }
}

fn percentile(sorted: &[u64], percentile: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * percentile).div_ceil(100).saturating_sub(1);
    sorted[rank.min(sorted.len() - 1)]
}

fn remote_call_config_summary(config: &Config) -> Vec<RemoteCallConfigSummary> {
    build_remote_call_report(config)
        .services
        .iter()
        .map(|service| RemoteCallConfigSummary {
            service: service.service_name(),
            status: service.status_name(),
            policy: service.policy_name(),
            endpoint_configured: service.endpoint.is_some(),
        })
        .collect()
}

fn sqlite_artifact_bytes(db_path: &Path) -> u64 {
    let mut total = fs::metadata(db_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        total += fs::metadata(PathBuf::from(format!("{}{suffix}", db_path.display())))
            .map(|metadata| metadata.len())
            .unwrap_or(0);
    }
    total
}

#[cfg(target_os = "linux")]
fn current_rss_bytes() -> Option<u64> {
    let statm = fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages = statm.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    // SAFETY: `sysconf(_SC_PAGESIZE)` is a read-only libc query with no pointer
    // arguments; negative return values are handled as unavailable.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return None;
    }
    Some(resident_pages.saturating_mul(page_size as u64))
}

#[cfg(not(target_os = "linux"))]
fn current_rss_bytes() -> Option<u64> {
    None
}

fn write_report(path: &Path, report: &BenchmarkMatrixReport) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create report directory {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(report)
        .context("failed to serialize benchmark matrix report")?;
    fs::write(path, json)
        .with_context(|| format!("failed to write benchmark report {}", path.display()))?;
    Ok(())
}

fn print_plain_report(report: &BenchmarkMatrixReport) {
    println!("mempal benchmark matrix");
    println!(
        "dataset: {} version={} source={} records={} queries={}",
        report.dataset.id,
        report.dataset.version,
        report.dataset.source,
        report.dataset.records,
        report.dataset.queries
    );
    println!(
        "reproducible: deterministic={} provider_calls_enabled={}",
        report.reproducibility.deterministic, report.reproducibility.provider_calls_enabled
    );
    for run in &report.runs {
        println!();
        println!("mode: {}", run.mode.as_str());
        println!(
            "engine: {} provider_execution={} top_k={}",
            run.retrieval_engine, run.provider_execution, run.top_k
        );
        println!(
            "recall@{}: {:.3} ({}/{})",
            run.recall.k,
            run.recall.recall_at_k,
            run.recall.hit_queries,
            run.recall.evaluated_queries
        );
        println!(
            "citation_correctness@{}: {:.3} ({}/{})",
            run.top_k,
            run.citation.correctness_at_k,
            run.citation.correct_queries,
            run.citation.evaluated_queries
        );
        println!(
            "leakage_rate@{}: {:.3} ({}/{})",
            run.top_k,
            run.leakage.leakage_rate_at_k,
            run.leakage.leaked_queries,
            run.leakage.evaluated_queries
        );
        println!(
            "stale_false_positive_rate@{}: {:.3} ({}/{})",
            run.top_k,
            run.stale_decision.false_positive_rate_at_k,
            run.stale_decision.false_positive_queries,
            run.stale_decision.evaluated_queries
        );
        println!(
            "latency_us: min={} p50={} p95={} max={} mean={:.1}",
            run.latency.min, run.latency.p50, run.latency.p95, run.latency.max, run.latency.mean
        );
        println!(
            "resources: rss_bytes={} scratch_sqlite_bytes={}",
            run.resources
                .rss_bytes
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unavailable".to_string()),
            run.resources.scratch_sqlite_bytes
        );
        println!(
            "remote_calls: embedding={} llm={} rerank={} total={} estimated_cost_usd={:.6}",
            run.remote_calls.embedding,
            run.remote_calls.llm,
            run.remote_calls.rerank,
            run.remote_calls.total,
            run.remote_calls.estimated_cost_usd
        );
    }
}

fn load_fixture(dataset: BenchmarkMatrixDataset) -> Fixture {
    match dataset {
        BenchmarkMatrixDataset::Builtin => builtin_fixture(),
    }
}

fn builtin_fixture() -> Fixture {
    Fixture {
        records: vec![
            FixtureRecord {
                drawer_id: "bench_matrix_alpha_sqlite_current",
                content: "Current decision: Project Alpha uses SQLite for durable memory storage and citation audit trails.",
                project_id: "project-alpha",
                source_file: "fixture://benchmark-matrix/project-alpha/sqlite-current.md",
                added_at: "2026-06-22T00:00:00Z",
                valid_until: None,
            },
            FixtureRecord {
                drawer_id: "bench_matrix_alpha_yaml_stale",
                content: "Old decision: Project Alpha uses YAML files for durable memory storage. This decision expired.",
                project_id: "project-alpha",
                source_file: "fixture://benchmark-matrix/project-alpha/yaml-stale.md",
                added_at: "2024-01-01T00:00:00Z",
                valid_until: Some("2025-01-01T00:00:00Z"),
            },
            FixtureRecord {
                drawer_id: "bench_matrix_alpha_citation",
                content: "Citation policy: Project Alpha memory answers must include drawer_id and source_file evidence.",
                project_id: "project-alpha",
                source_file: "fixture://benchmark-matrix/project-alpha/citation-policy.md",
                added_at: "2026-06-22T00:00:00Z",
                valid_until: None,
            },
            FixtureRecord {
                drawer_id: "bench_matrix_beta_redis",
                content: "Project Beta uses Redis for memory storage and belongs to a different project.",
                project_id: "project-beta",
                source_file: "fixture://benchmark-matrix/project-beta/redis.md",
                added_at: "2026-06-22T00:00:00Z",
                valid_until: None,
            },
        ],
        queries: vec![
            FixtureQuery {
                query: "Project Alpha SQLite durable memory storage",
                project_id: "project-alpha",
                expected_drawer_ids: &["bench_matrix_alpha_sqlite_current"],
                expected_source_files: &[
                    "fixture://benchmark-matrix/project-alpha/sqlite-current.md",
                ],
                stale_drawer_ids: &[],
                leakage_check: true,
            },
            FixtureQuery {
                query: "Project Alpha citation drawer_id source_file evidence",
                project_id: "project-alpha",
                expected_drawer_ids: &["bench_matrix_alpha_citation"],
                expected_source_files: &[
                    "fixture://benchmark-matrix/project-alpha/citation-policy.md",
                ],
                stale_drawer_ids: &[],
                leakage_check: true,
            },
            FixtureQuery {
                query: "current Project Alpha durable memory storage decision",
                project_id: "project-alpha",
                expected_drawer_ids: &["bench_matrix_alpha_sqlite_current"],
                expected_source_files: &[
                    "fixture://benchmark-matrix/project-alpha/sqlite-current.md",
                ],
                stale_drawer_ids: &["bench_matrix_alpha_yaml_stale"],
                leakage_check: true,
            },
            FixtureQuery {
                query: "Project Beta Redis memory storage",
                project_id: "project-alpha",
                expected_drawer_ids: &[],
                expected_source_files: &[],
                stale_drawer_ids: &[],
                leakage_check: true,
            },
        ],
    }
}
