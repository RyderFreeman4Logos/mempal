use super::*;

/// Regression guard: every property schema emitted in `tools/list` must be
/// a JSON object, never a bare boolean. Claude Code's MCP client rejects
/// the entire tool list when any property schema is a boolean `true`.
///
/// Specifically guards `mempal_phase3.metadata` and `.report`, which were
/// emitting `true` because schemars 1.x generates boolean schemas for
/// `serde_json::Value` fields.
#[test]
fn test_no_boolean_property_schemas_in_tools_list() {
    let (_tempdir, _db_path, server) = setup_server();
    let tools = server.tool_router.list_all();

    let mut violations: Vec<String> = Vec::new();

    for tool in &tools {
        if let Some(serde_json::Value::Object(props)) = tool.input_schema.get("properties") {
            for (prop_name, schema) in props {
                if schema.is_boolean() {
                    violations.push(format!("{}.{}", tool.name, prop_name));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "tools/list contains boolean property schemas (Claude Code rejects these):\n  {}",
        violations.join("\n  ")
    );

    // Specific regression: mempal_phase3.metadata and .report must be objects.
    let phase3 = tools
        .iter()
        .find(|t| t.name == "mempal_phase3")
        .expect("mempal_phase3 tool must be registered");
    let props = phase3
        .input_schema
        .get("properties")
        .and_then(|v| v.as_object())
        .expect("mempal_phase3 must have a properties object");

    assert!(
        props
            .get("metadata")
            .map(|v| v.is_object())
            .unwrap_or(false),
        "mempal_phase3.metadata property schema must be a JSON object, got: {:?}",
        props.get("metadata")
    );
    assert!(
        props.get("report").map(|v| v.is_object()).unwrap_or(false),
        "mempal_phase3.report property schema must be a JSON object, got: {:?}",
        props.get("report")
    );
}
