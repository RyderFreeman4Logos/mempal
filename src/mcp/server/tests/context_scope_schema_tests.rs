use super::setup_server;

#[test]
fn test_mcp_context_scope_schema_excludes_search_only_fields() {
    let (_tempdir, _db_path, server) = setup_server();
    let context_tool = server
        .tool_router
        .list_all()
        .into_iter()
        .find(|tool| tool.name == "mempal_context")
        .expect("mempal_context tool exists");
    let context_properties = context_tool
        .input_schema
        .get("properties")
        .and_then(|value| value.as_object())
        .expect("mempal_context must have a properties object");
    let context_scope_schema = context_properties
        .get("scope")
        .expect("mempal_context must expose a scope schema");
    let context_scope_definition_name = context_scope_schema
        .get("anyOf")
        .and_then(|variants| variants.as_array())
        .and_then(|variants| {
            variants.iter().find_map(|variant| {
                variant
                    .get("$ref")
                    .and_then(|value| value.as_str())
                    .and_then(|reference| reference.strip_prefix("#/$defs/"))
            })
        })
        .expect("mempal_context scope must reference a schema definition");
    let context_scope_schema = context_tool
        .input_schema
        .get("$defs")
        .and_then(|definitions| definitions.get(context_scope_definition_name))
        .expect("mempal_context scope definition must exist");
    let context_scope_properties = context_scope_schema
        .get("properties")
        .and_then(|value| value.as_object())
        .expect("mempal_context scope definition must have a properties object");

    for field in [
        "wing",
        "room",
        "session",
        "include_global",
        "memory_kind",
        "tier",
        "status",
        "anchor_kind",
    ] {
        assert!(
            !context_scope_properties.contains_key(field),
            "mempal_context scope must not expose search-only {field}"
        );
    }
    assert_eq!(
        context_scope_schema
            .get("additionalProperties")
            .and_then(|value| value.as_bool()),
        Some(false),
        "mempal_context scope must reject fields outside its schema"
    );
}
