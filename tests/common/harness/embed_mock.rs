//! Pauseable loopback HTTP server exposing an OpenAI-compatible
//! `POST /v1/embeddings` endpoint, implemented with `hyper` + `tokio`.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use anyhow::Result;
use hyper::body::to_bytes;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailMode {
    Ok,
    Timeout,
    Http500,
    RateLimit429,
}

struct Inner {
    dim: AtomicU32,
    paused: AtomicBool,
    pause_notify: Notify,
    fail_mode: Mutex<FailMode>,
    per_item_dims: Mutex<Option<Vec<u32>>>,
    request_count: AtomicU32,
    request_times: Mutex<Vec<Duration>>,
    start_at: tokio::time::Instant,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
}

#[derive(Clone)]
pub struct MockEmbedHandle {
    inner: Arc<Inner>,
}

impl MockEmbedHandle {
    pub fn pause(&self) {
        self.inner.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.inner.paused.store(false, Ordering::SeqCst);
        self.inner.pause_notify.notify_waiters();
    }

    pub fn set_dim(&self, dim: u32) {
        self.inner.dim.store(dim, Ordering::SeqCst);
    }

    pub async fn set_fail_mode(&self, mode: FailMode) {
        *self.inner.fail_mode.lock().await = mode;
    }

    pub fn request_count(&self) -> u32 {
        self.inner.request_count.load(Ordering::SeqCst)
    }

    pub async fn set_per_item_dims(&self, dims: Option<Vec<u32>>) {
        *self.inner.per_item_dims.lock().await = dims;
    }

    pub async fn request_times(&self) -> Vec<Duration> {
        self.inner.request_times.lock().await.clone()
    }

    pub async fn shutdown(&self) {
        if let Some(tx) = self.inner.shutdown_tx.lock().await.take() {
            let _ = tx.send(());
        }
    }
}

pub async fn start(port: u16) -> Result<(SocketAddr, MockEmbedHandle)> {
    let inner = Arc::new(Inner {
        dim: AtomicU32::new(4),
        paused: AtomicBool::new(false),
        pause_notify: Notify::new(),
        fail_mode: Mutex::new(FailMode::Ok),
        per_item_dims: Mutex::new(None),
        request_count: AtomicU32::new(0),
        request_times: Mutex::new(Vec::new()),
        start_at: tokio::time::Instant::now(),
        shutdown_tx: Mutex::new(None),
    });

    let handle = MockEmbedHandle {
        inner: Arc::clone(&inner),
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let make_service = make_service_fn(move |_| {
        let inner = Arc::clone(&inner);
        async move {
            Ok::<_, Infallible>(service_fn(move |request| {
                let inner = Arc::clone(&inner);
                async move { handle_request(inner, request).await }
            }))
        }
    });

    let server = Server::try_bind(&addr)?;
    let bind_addr = server.local_addr();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    *handle.inner.shutdown_tx.lock().await = Some(shutdown_tx);

    tokio::spawn(async move {
        let _ = server
            .serve(make_service)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
    });

    Ok((bind_addr, handle))
}

async fn handle_request(
    inner: Arc<Inner>,
    request: Request<Body>,
) -> Result<Response<Body>, Infallible> {
    inner.request_count.fetch_add(1, Ordering::SeqCst);
    inner
        .request_times
        .lock()
        .await
        .push(inner.start_at.elapsed());

    if request.method() != Method::POST || request.uri().path() != "/v1/embeddings" {
        return Ok(response(
            StatusCode::NOT_FOUND,
            json!({"error": "not found"}),
        ));
    }

    while inner.paused.load(Ordering::SeqCst) {
        inner.pause_notify.notified().await;
    }

    match *inner.fail_mode.lock().await {
        FailMode::Ok => {}
        FailMode::Timeout => {
            std::future::pending::<()>().await;
            unreachable!();
        }
        FailMode::Http500 => {
            return Ok(response(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "mock 500"}),
            ));
        }
        FailMode::RateLimit429 => {
            return Ok(response(
                StatusCode::TOO_MANY_REQUESTS,
                json!({"error": "mock 429"}),
            ));
        }
    }

    let body = to_bytes(request.into_body()).await.unwrap_or_default();
    let payload: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let count = match payload.get("input") {
        Some(Value::Array(values)) => values.len(),
        Some(_) => 1,
        None => 1,
    };
    let default_dim = inner.dim.load(Ordering::SeqCst) as usize;
    let per_item_dims = inner.per_item_dims.lock().await.clone();
    let data = (0..count)
        .map(|index| {
            let dim = per_item_dims
                .as_ref()
                .and_then(|dims| dims.get(index).copied())
                .unwrap_or(default_dim as u32) as usize;
            let embedding = vec![0.0_f32; dim];
            json!({
                "index": index,
                "embedding": embedding,
            })
        })
        .collect::<Vec<_>>();

    Ok(response(StatusCode::OK, json!({ "data": data })))
}

fn response(status: StatusCode, body: Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .expect("build mock embed response")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn smoke_mock_server_responds() {
        let (addr, handle) = start(0).await.expect("start mock server");
        handle.set_dim(6);
        let response = reqwest::Client::new()
            .post(format!("http://{addr}/v1/embeddings"))
            .json(&json!({"input": ["hello"]}))
            .send()
            .await
            .expect("send request");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("parse response body");
        assert_eq!(
            body["data"][0]["embedding"]
                .as_array()
                .expect("embedding")
                .len(),
            6
        );

        handle.shutdown().await;
    }
}
