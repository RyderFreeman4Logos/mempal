//! One-shot OpenAI embeddings stub that waits for the owned client or stop.

use std::io::Write;
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

#[path = "bounded_http_request.rs"]
mod bounded_http_request;

pub fn start(
    expected_query: &str,
    vector: Vec<f32>,
) -> (String, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding stub");
    listener
        .set_nonblocking(true)
        .expect("set embedding stub nonblocking");
    let address = listener.local_addr().expect("local addr");
    let expected_query = expected_query.to_string();
    let stop_for_thread = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let (mut stream, _) = loop {
            if stop_for_thread.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("accept request: {error}"),
            }
        };
        let request = bounded_http_request::read_tcp(&mut stream, bounded_http_request::MAX_BYTES)
            .expect("read embedding request");
        let (headers, body) = request
            .split_once("\r\n\r\n")
            .expect("request should contain HTTP headers and JSON body");
        let request_line = headers.lines().next().expect("request line");
        assert_eq!(request_line, "POST /v1/embeddings HTTP/1.1");

        let payload: Value = serde_json::from_str(body).expect("parse embedding request body");
        assert_eq!(payload["model"], "test-model");
        let input = payload["input"]
            .as_array()
            .expect("input should be an array");
        assert_eq!(input.len(), 1, "expected a single embedding query");
        assert_eq!(input[0], expected_query);

        let body = serde_json::to_string(&json!({
            "data": [{ "embedding": vector }]
        }))
        .expect("serialize response body");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write embedding response");
    });

    (format!("http://{address}/v1"), stop, handle)
}
