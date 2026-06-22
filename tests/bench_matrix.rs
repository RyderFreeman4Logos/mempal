use mempal::bench_matrix::{
    BenchmarkMatrixArgs, BenchmarkMatrixDataset, BenchmarkMatrixFormat, BenchmarkMatrixMode,
    BenchmarkMatrixModeArg, run_benchmark_matrix,
};
use mempal::core::config::Config;

fn args(mode: BenchmarkMatrixModeArg) -> BenchmarkMatrixArgs {
    BenchmarkMatrixArgs {
        dataset: BenchmarkMatrixDataset::Builtin,
        mode,
        top_k: 5,
        format: BenchmarkMatrixFormat::Json,
        out: None,
    }
}

#[test]
fn default_matrix_reports_required_no_llm_metrics_without_provider_calls() {
    let report = run_benchmark_matrix(&Config::default(), args(BenchmarkMatrixModeArg::NoLlm))
        .expect("matrix benchmark should run");

    assert_eq!(report.schema_version, "mempal.benchmark_matrix.v1");
    assert_eq!(report.runs.len(), 1);
    let run = &report.runs[0];
    assert_eq!(run.mode, BenchmarkMatrixMode::NoLlm);
    assert_eq!(run.recall.evaluated_queries, 3);
    assert!(run.recall.recall_at_k > 0.0);
    assert_eq!(run.citation.evaluated_queries, 3);
    assert_eq!(run.leakage.evaluated_queries, 4);
    assert_eq!(run.remote_calls.total, 0);
    assert_eq!(run.remote_calls.estimated_cost_usd, 0.0);
    assert!(!report.reproducibility.provider_calls_enabled);
}

#[test]
fn all_mode_distinguishes_no_local_and_cloud_without_provider_calls() {
    let report = run_benchmark_matrix(&Config::default(), args(BenchmarkMatrixModeArg::All))
        .expect("matrix benchmark should run");

    let modes = report.runs.iter().map(|run| run.mode).collect::<Vec<_>>();
    assert_eq!(
        modes,
        vec![
            BenchmarkMatrixMode::NoLlm,
            BenchmarkMatrixMode::LocalLlm,
            BenchmarkMatrixMode::CloudLlm,
        ]
    );
    assert!(report.runs.iter().all(|run| run.remote_calls.total == 0));
    assert!(
        report
            .runs
            .iter()
            .all(|run| run.provider_execution == "disabled_deterministic_fixture")
    );
}

#[test]
fn json_report_omits_raw_fixture_memory_contents() {
    let report = run_benchmark_matrix(&Config::default(), args(BenchmarkMatrixModeArg::NoLlm))
        .expect("matrix benchmark should run");
    let json = serde_json::to_string(&report).expect("serialize");

    assert!(!json.contains("Current decision"));
    assert!(!json.contains("Project Beta uses Redis"));
    assert!(!json.contains("fixture://benchmark-matrix/project-alpha/sqlite-current.md"));
}
