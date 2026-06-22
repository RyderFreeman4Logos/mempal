use toml::Value;

fn cargo_manifest() -> Value {
    include_str!("../Cargo.toml")
        .parse::<Value>()
        .expect("Cargo.toml must parse")
}

fn feature_entries(name: &str) -> Vec<String> {
    cargo_manifest()
        .get("features")
        .and_then(|features| features.get(name))
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("feature {name} must be defined"))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("feature {name} contains non-string entry"))
                .to_string()
        })
        .collect()
}

fn assert_feature_does_not_enable_model2vec(name: &str) {
    let entries = feature_entries(name);
    assert!(
        !entries.iter().any(|entry| entry == "model2vec"),
        "{name} feature must not enable model2vec implicitly: {entries:?}"
    );
    assert!(
        !entries.iter().any(|entry| entry == "dep:model2vec-rs"),
        "{name} feature must not enable model2vec-rs directly: {entries:?}"
    );
}

#[test]
fn default_feature_set_does_not_enable_model2vec() {
    assert_feature_does_not_enable_model2vec("default");
}

#[test]
fn rest_feature_does_not_enable_model2vec() {
    assert_feature_does_not_enable_model2vec("rest");
}
