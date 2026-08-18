use mempal::cited_recall_bench::{
    cited_recall_bench_passes, run_cited_recall_bench, run_cited_recall_bench_command,
};

#[test]
fn cited_recall_resume_compaction_gate_passes_without_leaking_fixture_text() {
    let report = run_cited_recall_bench().expect("cited recall bench should run");

    assert_eq!(report.schema_version, "mempal.cited_recall_bench.v1");
    assert_eq!(report.dataset.id, "cited_recall_resume_compaction_v1");
    assert!(cited_recall_bench_passes(&report), "{report:?}");
    assert_eq!(report.remote_calls, 0);
    assert!(!report.reproducibility.provider_calls_enabled);

    let json = serde_json::to_string(&report).expect("serialize report");
    assert!(!json.contains("MempalAlpha"));
    assert!(!json.contains("YAML files"));
    assert!(!json.contains("Redis"));
    assert!(!json.contains("ad-hoc notes"));
    assert!(!json.contains("fixture://cited-recall/"));
}

#[test]
fn cited_recall_command_exits_zero_for_passing_gate() {
    run_cited_recall_bench_command().expect("focused cited-recall command should pass");
}
