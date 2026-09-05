//! Deterministic HTTP framing regressions for the context embedding stub (#1082).

use std::io::{self, Cursor, Read};

#[path = "support/bounded_http_request.rs"]
mod bounded_http_request;

const _: fn(&mut std::net::TcpStream, usize) -> std::io::Result<String> =
    bounded_http_request::read_tcp;

struct Chunks {
    parts: Vec<Vec<u8>>,
}

impl Read for Chunks {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(part) = self.parts.first() else {
            return Ok(0);
        };
        if part.is_empty() {
            self.parts.remove(0);
            return Ok(0);
        }
        let n = part.len().min(buf.len());
        buf[..n].copy_from_slice(&part[..n]);
        if n == part.len() {
            self.parts.remove(0);
        } else {
            self.parts[0].drain(..n);
        }
        Ok(n)
    }
}

fn http_request(body: &str) -> Vec<u8> {
    let mut bytes = format!(
        "POST /v1/embeddings HTTP/1.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

#[test]
fn fragmented_request_assembles_headers_then_body() {
    let body = r#"{"model":"test-model","input":["debug"]}"#;
    let bytes = http_request(body);
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header delimiter")
        + 4;
    let mut stream = Chunks {
        parts: vec![bytes[..split].to_vec(), bytes[split..].to_vec()],
    };
    let request = bounded_http_request::read(&mut stream, bounded_http_request::MAX_BYTES)
        .expect("complete fragmented request");
    let (headers, got_body) = request
        .split_once("\r\n\r\n")
        .expect("headers and JSON body");
    assert_eq!(
        headers.lines().next().expect("request line"),
        "POST /v1/embeddings HTTP/1.1"
    );
    let payload: serde_json::Value = serde_json::from_str(got_body).expect("json body");
    assert_eq!(payload["model"], "test-model");
    assert_eq!(payload["input"][0], "debug");
}

#[test]
fn truncated_body_returns_eof() {
    let bytes = http_request(r#"{"model":"test-model"}"#);
    let mut stream = Cursor::new(&bytes[..bytes.len() - 4]);
    let error = bounded_http_request::read(&mut stream, bounded_http_request::MAX_BYTES)
        .expect_err("truncated body");
    assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
}

#[test]
fn oversized_content_length_is_rejected() {
    let request = b"POST /v1/embeddings HTTP/1.1\r\ncontent-length: 99999\r\n\r\n{}";
    let error =
        bounded_http_request::read(&mut Cursor::new(&request[..]), 64).expect_err("oversized");
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}
