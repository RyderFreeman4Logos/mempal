use mempal::core::config::Config;
use mempal::endpoint_health::probe_endpoints;
use mockito::Server;

#[tokio::test]
async fn test_endpoint_health_probes_llm_endpoint_pool() {
    let mut primary = Server::new_async().await;
    let mut secondary = Server::new_async().await;
    let primary_mock = primary
        .mock("GET", "/v1/models")
        .with_status(500)
        .with_body("primary unavailable")
        .create_async()
        .await;
    let secondary_mock = secondary
        .mock("GET", "/v1/models")
        .with_status(200)
        .with_body(r#"{"object":"list","data":[]}"#)
        .create_async()
        .await;
    let config = Config::parse(&format!(
        r#"
[llm]
enabled = true

[[llm.endpoints]]
id = "primary"
base_url = "{}/v1"
model = "primary-model"

[[llm.endpoints]]
id = "secondary"
base_url = "{}/v1"
model = "secondary-model"
"#,
        primary.url(),
        secondary.url()
    ))
    .expect("parse endpoint pool");

    let health = probe_endpoints(&config).await;

    primary_mock.assert_async().await;
    secondary_mock.assert_async().await;
    assert!(health.llm.reachable, "{health:#?}");
    assert_eq!(health.llm.detail, "http probe via secondary");
}
