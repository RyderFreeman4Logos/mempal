use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

pub(crate) const HOOK_IPC_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const HOOK_IPC_READ_TIMEOUT: Duration = Duration::from_secs(2);

const SOCKET_FILE_NAME: &str = "daemon-hook.sock";
const MAX_IPC_FRAME_BYTES: usize = 12 * 1024 * 1024;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
const IPC_READ_CHUNK_BYTES: usize = 8 * 1024;

static ATTEMPT_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct HookIpcEnqueueRequest {
    pub kind: String,
    pub payload: String,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum HookIpcEnqueueResponse {
    Accepted,
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookIpcClientOutcome {
    Accepted,
    Fallback(HookIpcFallbackReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HookIpcFallbackReason {
    SocketUnavailable,
    RuntimeInitFailed(String),
    ConnectFailed(String),
    Timeout,
    RequestEncodeFailed(String),
    RequestWriteFailed(String),
    Rejected(String),
    ResponseReadFailed(String),
}

pub(crate) struct SocketFileGuard {
    path: PathBuf,
}

impl fmt::Display for HookIpcFallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SocketUnavailable => write!(f, "daemon IPC socket unavailable"),
            Self::RuntimeInitFailed(error) => write!(f, "daemon IPC runtime init failed: {error}"),
            Self::ConnectFailed(error) => write!(f, "daemon IPC connect failed: {error}"),
            Self::Timeout => write!(f, "daemon IPC timed out"),
            Self::RequestEncodeFailed(error) => {
                write!(f, "daemon IPC request encode failed: {error}")
            }
            Self::RequestWriteFailed(error) => {
                write!(f, "daemon IPC request write failed: {error}")
            }
            Self::Rejected(message) => write!(f, "daemon IPC rejected payload: {message}"),
            Self::ResponseReadFailed(error) => {
                write!(f, "daemon IPC response read failed: {error}")
            }
        }
    }
}

impl HookIpcFallbackReason {
    pub(crate) fn may_have_reached_daemon(&self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::RequestWriteFailed(_) | Self::ResponseReadFailed(_)
        )
    }
}

impl SocketFileGuard {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl HookIpcEnqueueRequest {
    pub(crate) fn new(kind: &str, payload: &str) -> Self {
        Self {
            kind: kind.to_string(),
            payload: payload.to_string(),
            idempotency_key: new_idempotency_key(),
        }
    }
}

impl Drop for SocketFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) fn socket_path(mempal_home: &Path) -> PathBuf {
    mempal_home.join(SOCKET_FILE_NAME)
}

pub(crate) fn bind_listener(mempal_home: &Path) -> Result<(UnixListener, SocketFileGuard)> {
    let path = socket_path(mempal_home);
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove stale {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(&path).with_context(|| format!("failed to bind {}", path.display()))?;
    Ok((listener, SocketFileGuard { path }))
}

pub(crate) fn enqueue_with_default_timeout(
    mempal_home: &Path,
    request: HookIpcEnqueueRequest,
) -> HookIpcClientOutcome {
    enqueue_with_timeout(mempal_home, request, HOOK_IPC_TIMEOUT)
}

pub(crate) fn enqueue_with_timeout(
    mempal_home: &Path,
    request: HookIpcEnqueueRequest,
    timeout: Duration,
) -> HookIpcClientOutcome {
    let path = socket_path(mempal_home);
    if !path.exists() {
        return HookIpcClientOutcome::Fallback(HookIpcFallbackReason::SocketUnavailable);
    }

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return HookIpcClientOutcome::Fallback(HookIpcFallbackReason::RuntimeInitFailed(
                error.to_string(),
            ));
        }
    };

    runtime.block_on(async move {
        match tokio::time::timeout(timeout, enqueue_once(&path, request)).await {
            Ok(outcome) => outcome,
            Err(_) => HookIpcClientOutcome::Fallback(HookIpcFallbackReason::Timeout),
        }
    })
}

async fn enqueue_once(path: &Path, request: HookIpcEnqueueRequest) -> HookIpcClientOutcome {
    let mut stream = match UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(error) => {
            return HookIpcClientOutcome::Fallback(HookIpcFallbackReason::ConnectFailed(
                error.to_string(),
            ));
        }
    };
    let mut frame = match serde_json::to_vec(&request) {
        Ok(frame) => frame,
        Err(error) => {
            return HookIpcClientOutcome::Fallback(HookIpcFallbackReason::RequestEncodeFailed(
                error.to_string(),
            ));
        }
    };
    frame.push(b'\n');
    if let Err(error) = stream.write_all(&frame).await {
        return HookIpcClientOutcome::Fallback(HookIpcFallbackReason::RequestWriteFailed(
            error.to_string(),
        ));
    }
    if let Err(error) = stream.flush().await {
        return HookIpcClientOutcome::Fallback(HookIpcFallbackReason::RequestWriteFailed(
            error.to_string(),
        ));
    }

    match read_enqueue_response(&mut stream).await {
        Ok(HookIpcEnqueueResponse::Accepted) => HookIpcClientOutcome::Accepted,
        Ok(HookIpcEnqueueResponse::Error { message }) => {
            HookIpcClientOutcome::Fallback(HookIpcFallbackReason::Rejected(message))
        }
        Err(error) => HookIpcClientOutcome::Fallback(HookIpcFallbackReason::ResponseReadFailed(
            error.to_string(),
        )),
    }
}

pub(crate) async fn read_enqueue_request(stream: &mut UnixStream) -> Result<HookIpcEnqueueRequest> {
    let frame = read_frame(stream).await?;
    let request: HookIpcEnqueueRequest = serde_json::from_slice(trim_line_ending(&frame))
        .context("invalid hook IPC request JSON")?;
    if request.kind.trim().is_empty() {
        bail!("hook IPC request kind must not be empty");
    }
    if request.payload.is_empty() {
        bail!("hook IPC request payload must not be empty");
    }
    if request.idempotency_key.trim().is_empty() {
        bail!("hook IPC request idempotency_key must not be empty");
    }
    if request.idempotency_key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        bail!(
            "hook IPC request idempotency_key exceeds {} bytes",
            MAX_IDEMPOTENCY_KEY_BYTES
        );
    }
    Ok(request)
}

pub(crate) async fn write_enqueue_response(
    stream: &mut UnixStream,
    response: &HookIpcEnqueueResponse,
) -> Result<()> {
    let mut frame =
        serde_json::to_vec(response).context("failed to serialize hook IPC response")?;
    frame.push(b'\n');
    stream
        .write_all(&frame)
        .await
        .context("failed to write hook IPC response")?;
    stream
        .flush()
        .await
        .context("failed to flush hook IPC response")?;
    Ok(())
}

async fn read_enqueue_response(stream: &mut UnixStream) -> Result<HookIpcEnqueueResponse> {
    let frame = read_frame(stream).await?;
    serde_json::from_slice(trim_line_ending(&frame)).context("invalid hook IPC response JSON")
}

async fn read_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    read_frame_with_limit(stream, MAX_IPC_FRAME_BYTES).await
}

async fn read_frame_with_limit(stream: &mut UnixStream, max_frame_bytes: usize) -> Result<Vec<u8>> {
    if max_frame_bytes == 0 {
        bail!("hook IPC frame limit must be greater than zero");
    }

    let mut frame = Vec::new();
    let mut chunk = [0_u8; IPC_READ_CHUNK_BYTES];

    loop {
        let bytes = stream
            .read(&mut chunk)
            .await
            .context("failed to read hook IPC frame")?;
        if bytes == 0 {
            if frame.is_empty() {
                bail!("empty hook IPC frame");
            }
            bail!("unterminated hook IPC frame");
        }

        let received = &chunk[..bytes];
        let frame_part_len = received
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(received.len(), |delimiter_index| delimiter_index + 1);
        if frame.len() + frame_part_len > max_frame_bytes {
            bail!("hook IPC frame exceeds {} bytes", max_frame_bytes);
        }

        frame.extend_from_slice(&received[..frame_part_len]);
        if frame.ends_with(b"\n") {
            return Ok(frame);
        }
    }
}

fn trim_line_ending(frame: &[u8]) -> &[u8] {
    frame
        .strip_suffix(b"\n")
        .unwrap_or(frame)
        .strip_suffix(b"\r")
        .unwrap_or_else(|| frame.strip_suffix(b"\n").unwrap_or(frame))
}

fn new_idempotency_key() -> String {
    let now_ns = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_nanos(),
        Err(_) => 0,
    };
    let pid = std::process::id();
    let counter = ATTEMPT_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Hasher::new();
    hasher.update(&now_ns.to_le_bytes());
    hasher.update(&pid.to_le_bytes());
    hasher.update(&counter.to_le_bytes());
    format!("hook-ipc-{}", hasher.finalize().to_hex())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_frame_rejects_oversized_frame_before_extending_past_limit() {
        let (mut client, mut server) = UnixStream::pair().expect("unix stream pair");
        let reader = tokio::spawn(async move { read_frame_with_limit(&mut server, 32).await });

        let oversized = vec![b'a'; 33];
        let _ = client.write_all(&oversized).await;

        let error = reader
            .await
            .expect("reader task")
            .expect_err("oversized frame must fail");
        assert!(
            error
                .to_string()
                .contains("hook IPC frame exceeds 32 bytes"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_frame_rejects_unterminated_frame() {
        let (mut client, mut server) = UnixStream::pair().expect("unix stream pair");
        let reader = tokio::spawn(async move { read_frame_with_limit(&mut server, 32).await });

        client
            .write_all(b"{\"status\":\"accepted\"}")
            .await
            .expect("write frame");
        client.shutdown().await.expect("shutdown client");

        let error = reader
            .await
            .expect("reader task")
            .expect_err("unterminated frame must fail");
        assert!(
            error.to_string().contains("unterminated hook IPC frame"),
            "{error:#}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_read_frame_accepts_delimited_frame_at_limit() {
        let (mut client, mut server) = UnixStream::pair().expect("unix stream pair");
        let reader = tokio::spawn(async move { read_frame_with_limit(&mut server, 32).await });

        let mut frame = vec![b'a'; 31];
        frame.push(b'\n');
        client.write_all(&frame).await.expect("write frame");

        let read = reader.await.expect("reader task").expect("frame at limit");
        assert_eq!(read, frame);
    }
}
