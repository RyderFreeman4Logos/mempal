use crate::*;

#[test]
fn test_ingest_wait_json_admission_blocked_output_is_cleanup_safe_at_15_of_16_service_holders() {
    let home = setup_home();
    let db_path = home.path().join(".mempal/palace.db");
    let _holders = (0..15)
        .map(|_| {
            ProfileDbAdmission::acquire(&db_path, DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1))
                .expect("fill service holder baseline")
        })
        .collect::<Vec<_>>();

    let output = run_cli_with_stdin_bounded(
        home.path(),
        &[
            "ingest",
            "--stdin",
            "--wing",
            "smoke",
            "--source-type",
            "user_explicit",
            "--no-gate",
            "--wait",
            "--wait-timeout-secs",
            "5",
            "--json",
        ],
        br#"{"content":"admission-blocked JSON receipt must survive saturated service holders"}"#,
        Duration::from_secs(30),
    );

    assert!(
        !output.status.success(),
        "admission exhaustion must fail closed"
    );
    let stdout: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("admission-blocked receipt was not valid JSON"));
    assert_eq!(stdout["outcome"], "admission_blocked");
    assert_eq!(stdout["reason"], "holder_budget_exceeded");
    assert_eq!(stdout["action"], "write_refused");
    assert_eq!(stdout["capacity"]["holders"], 16);
    assert_eq!(stdout["profile_admission"]["service_holders"], 15);
    assert_eq!(stdout["headroom"]["holders"], 1);
    assert!(
        stdout["created_drawer_ids"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(
        stdout["cleanup_drawer_ids"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    assert!(
        cleanup_ids_from_ingest_json(&stdout).is_empty(),
        "blocked receipt exposed cleanup IDs"
    );
}
