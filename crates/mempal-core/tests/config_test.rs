use std::fs;

use mempal_core::config::Config;
use tempfile::tempdir;

#[test]
fn test_config_defaults() {
    let config = Config::default();

    assert_eq!(config.embed.backend, "onnx");
    assert_eq!(config.db_path, "~/.mempal/palace.db");
}

#[test]
fn test_config_load_from_file() {
    let dir = tempdir().expect("temp dir should be created");
    let path = dir.path().join("config.toml");

    fs::write(
        &path,
        r#"
[embed]
backend = "api"
api_endpoint = "http://localhost:11434"
"#,
    )
    .expect("config file should be written");

    let config = Config::load_from(&path).expect("config should load from file");

    assert_eq!(config.embed.backend, "api");
    assert_eq!(
        config.embed.api_endpoint.as_deref(),
        Some("http://localhost:11434")
    );
    assert_eq!(config.db_path, "~/.mempal/palace.db");
}
