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

/// Terminal outcome of the one-shot stub accept loop.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StubOutcome {
    Served,
    Stopped,
}

/// RAII owner: `Drop` stops and joins the accept thread on panic/error paths.
pub struct EmbeddingStub {
    endpoint: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<StubOutcome>>,
}

impl EmbeddingStub {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Stop the accept loop and join. Surfaces join errors on the success path.
    pub fn stop_and_join(mut self) -> StubOutcome {
        self.stop.store(true, Ordering::Relaxed);
        self.take_handle().join().expect("join embedding stub")
    }

    fn take_handle(&mut self) -> thread::JoinHandle<StubOutcome> {
        self.handle.take().expect("embedding stub already joined")
    }
}

impl Drop for EmbeddingStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn start(expected_query: &str, vector: Vec<f32>) -> EmbeddingStub {
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
                return StubOutcome::Stopped;
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
        StubOutcome::Served
    });

    EmbeddingStub {
        endpoint: format!("http://{address}/v1"),
        stop,
        handle: Some(handle),
    }
}

#[cfg(test)]
mod owner_tests {
    use super::{StubOutcome, start};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn listen_addr(endpoint: &str) -> String {
        endpoint
            .strip_prefix("http://")
            .and_then(|rest| rest.strip_suffix("/v1"))
            .expect("stub endpoint")
            .to_string()
    }

    fn embedding_request(query: &str) -> Vec<u8> {
        let body = format!(r#"{{"model":"test-model","input":["{query}"]}}"#);
        format!(
            "POST /v1/embeddings HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    #[test]
    fn stop_before_accept_is_stopped_not_served() {
        let stub = start("q", vec![0.25; 2]);
        let addr = listen_addr(stub.endpoint());
        let outcome = stub.stop_and_join();
        assert_eq!(outcome, StubOutcome::Stopped);
        assert!(
            TcpStream::connect(&addr).is_err(),
            "stopped stub must release the listen socket"
        );
    }

    #[test]
    fn delayed_client_request_is_served() {
        let query = "hello";
        let stub = start(query, vec![0.25; 2]);
        let addr = listen_addr(stub.endpoint());
        let mut stream = TcpStream::connect(&addr).expect("connect embedding stub");
        stream
            .write_all(&embedding_request(query))
            .expect("write embedding request");
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown write");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .expect("read embedding response");
        assert!(
            String::from_utf8_lossy(&response).starts_with("HTTP/1.1 200 OK"),
            "stub must answer the owned client"
        );
        let outcome = stub.stop_and_join();
        assert_eq!(outcome, StubOutcome::Served);
    }

    #[test]
    fn drop_stops_and_joins_after_panic() {
        let stub = start("q", vec![0.25; 2]);
        let addr = listen_addr(stub.endpoint());
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let _stub = stub;
            panic!("forced owner cleanup");
        }));
        assert!(panicked.is_err(), "fixture must unwind through Drop");
        assert!(
            TcpStream::connect(&addr).is_err(),
            "panic cleanup must join and release the listen socket"
        );
    }
}
