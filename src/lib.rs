//! proverd — TLSN Proxy-TLS prover sidecar for routstr-core.
//!
//! Channels (per design doc TLSN-routstr.md):
//! - B: verifier dials OUT via WebSocket to `GET /ws?session_id=...` (tlsn mux)
//! - C: verifier dials the upstream itself and relays ciphertext (not proverd's
//!   concern — in proxy mode proverd needs no upstream connectivity at all)
//! - API: routstr-core calls `POST /sessions` and receives the decrypted
//!   upstream response streamed back as it arrives.

pub mod fixtures;
pub mod testfixture;
pub mod ws_io;

pub mod testverifier;

use std::{
    collections::HashMap,
    future::IntoFuture,
    ops::Range,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State, WebSocketUpgrade},
    http::{HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use futures::{SinkExt, channel::mpsc, channel::oneshot};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use tracing::{debug, info, instrument, warn};

use tlsn::{
    Session,
    config::{
        prove::ProveConfig, prover::ProverConfig, tls::TlsClientConfig,
        tls_commit::proxy::ProxyTlsConfig,
    },
    connection::{DnsName, ServerName},
    webpki::RootCertStore,
};

/// proverd configuration.
#[derive(Clone)]
pub struct Config {
    /// Root certificates the prover trusts for upstream TLS sessions.
    pub root_store: RootCertStore,
    /// How long a `POST /sessions` waits for the verifier's WebSocket.
    pub ws_wait_timeout: Duration,
    /// Default cap on received transcript bytes.
    pub default_max_recv_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            root_store: RootCertStore::mozilla(),
            ws_wait_timeout: Duration::from_secs(30),
            default_max_recv_bytes: 1 << 20, // 1 MiB
        }
    }
}

/// Request body for `POST /sessions`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionRequest {
    /// Caller-chosen id pairing this call to the `/ws` verifier connection.
    pub session_id: String,
    /// Upstream host (SNI / certificate name), e.g. `api.openai.com`.
    pub server_name: String,
    /// Upstream port. Advisory in proxy mode — the verifier dials; logged and
    /// surfaced so the verifier can enforce policy.
    #[serde(default = "default_port")]
    pub port: u16,
    /// The HTTP request to author inside the TLS session.
    pub request: HttpRequestSpec,
    /// Header names whose VALUE bytes are redacted from the proof.
    #[serde(default = "default_redact")]
    pub redact: Vec<String>,
    /// Cap on received transcript bytes; session aborts past it.
    pub max_recv_bytes: Option<usize>,
}

fn default_port() -> u16 {
    443
}

fn default_redact() -> Vec<String> {
    vec!["authorization".to_string()]
}

/// HTTP request to author upstream.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpRequestSpec {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: String,
}

/// Status of a session, queryable via `GET /sessions/{id}/status`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionStatus {
    WaitingForVerifier,
    Running,
    Complete(SessionTimings),
    Failed { error: String },
}

/// Per-session timings (milliseconds) and transcript sizes.
#[derive(Debug, Clone, Serialize)]
pub struct SessionTimings {
    pub ws_wait_ms: u128,
    pub tls_setup_ms: u128,
    pub ttfb_ms: u128,
    pub transfer_ms: u128,
    pub proof_ms: u128,
    pub total_ms: u128,
    pub sent_bytes: usize,
    pub recv_bytes: usize,
}

/// Pairs `POST /sessions` calls with `/ws` verifier connections by session id.
#[derive(Default)]
struct Matcher {
    inner: Mutex<HashMap<String, MatchEntry>>,
}

enum MatchEntry {
    PostWaiting(oneshot::Sender<axum::extract::ws::WebSocket>),
    WsWaiting(axum::extract::ws::WebSocket),
}

impl Matcher {
    /// Called by the POST handler; resolves when the verifier's ws arrives.
    fn wait_for_ws(
        &self,
        session_id: &str,
    ) -> oneshot::Receiver<axum::extract::ws::WebSocket> {
        let mut inner = self.inner.lock().unwrap();
        match inner.remove(session_id) {
            Some(MatchEntry::WsWaiting(ws)) => {
                let (tx, rx) = oneshot::channel();
                let _ = tx.send(ws);
                rx
            }
            Some(MatchEntry::PostWaiting(_)) => {
                // Duplicate registration; replace.
                let (tx, rx) = oneshot::channel();
                inner.insert(session_id.to_string(), MatchEntry::PostWaiting(tx));
                rx
            }
            None => {
                let (tx, rx) = oneshot::channel();
                inner.insert(session_id.to_string(), MatchEntry::PostWaiting(tx));
                rx
            }
        }
    }

    /// Called by the ws handler once the upgrade completes.
    fn provide_ws(&self, session_id: &str, ws: axum::extract::ws::WebSocket) {
        let mut inner = self.inner.lock().unwrap();
        match inner.remove(session_id) {
            Some(MatchEntry::PostWaiting(tx)) => {
                if tx.send(ws).is_err() {
                    warn!(session_id, "POST side gone; dropping verifier ws");
                }
            }
            _ => {
                inner.insert(session_id.to_string(), MatchEntry::WsWaiting(ws));
            }
        }
    }
}

/// Shared application state.
pub struct AppState {
    pub config: Config,
    matcher: Matcher,
    statuses: Mutex<HashMap<String, SessionStatus>>,
}

impl AppState {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            matcher: Matcher::default(),
            statuses: Mutex::new(HashMap::new()),
        })
    }

    fn set_status(&self, session_id: &str, status: SessionStatus) {
        self.statuses
            .lock()
            .unwrap()
            .insert(session_id.to_string(), status);
    }

    pub fn status(&self, session_id: &str) -> Option<SessionStatus> {
        self.statuses.lock().unwrap().get(session_id).cloned()
    }
}

/// Builds the HTTP router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sessions", post(create_session))
        .route("/sessions/{id}/status", get(session_status))
        .route("/ws", get(ws_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}

async fn session_status(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match state.status(&id) {
        Some(status) => Json(status).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ws_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(session_id) = params.get("session_id").cloned() else {
        return (StatusCode::BAD_REQUEST, "missing session_id").into_response();
    };
    info!(session_id, "verifier websocket connecting");
    ws.on_upgrade(move |socket| async move {
        state.matcher.provide_ws(&session_id, socket);
    })
}

/// Head of the upstream response, forwarded to the POST handler so it can
/// start streaming the body to routstr-core.
struct RespHead {
    status: u16,
    headers: Vec<(String, String)>,
}

async fn create_session(
    State(state): State<Arc<AppState>>,
    Json(req): Json<SessionRequest>,
) -> Response {
    let session_id = req.session_id.clone();
    info!(session_id, server = %req.server_name, "new session request");

    state.set_status(&session_id, SessionStatus::WaitingForVerifier);

    let (head_tx, head_rx) = oneshot::channel::<Result<RespHead, String>>();
    // Body chunk channel: futures mpsc implements Stream for axum's
    // Body::from_stream.
    let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(64);

    let task_state = state.clone();
    let task_session_id = session_id.clone();
    tokio::spawn(async move {
        let session_id = task_session_id;
        let wait_started = Instant::now();
        let ws = match tokio::time::timeout(
            task_state.config.ws_wait_timeout,
            task_state.matcher.wait_for_ws(&session_id),
        )
        .await
        {
            Ok(Ok(ws)) => ws,
            Ok(Err(_)) => {
                let _ = head_tx.send(Err("ws matcher channel closed".into()));
                task_state.set_status(
                    &session_id,
                    SessionStatus::Failed {
                        error: "ws matcher channel closed".into(),
                    },
                );
                return;
            }
            Err(_) => {
                let _ = head_tx.send(Err("timed out waiting for verifier websocket".into()));
                task_state.set_status(
                    &session_id,
                    SessionStatus::Failed {
                        error: "timed out waiting for verifier websocket".into(),
                    },
                );
                return;
            }
        };
        let ws_wait = wait_started.elapsed();
        task_state.set_status(&session_id, SessionStatus::Running);

        let io = ws_io::ws_to_io(ws);
        if let Err(e) = run_session(&task_state, req, io, ws_wait, head_tx, body_tx).await {
            warn!(session_id, error = %e, "session failed");
            task_state.set_status(
                &session_id,
                SessionStatus::Failed {
                    error: format!("{e:#}"),
                },
            );
        }
    });

    match head_rx.await {
        Ok(Ok(head)) => {
            let mut builder = Response::builder()
                .status(head.status)
                .header("x-tlsn-session", &session_id)
                .header("x-tlsn-prover", "proverd/0.1 (tlsn proxy-tls)");
            for (name, value) in &head.headers {
                if let (Ok(name), Ok(value)) = (
                    name.parse::<HeaderName>(),
                    HeaderValue::from_str(value),
                ) {
                    builder = builder.header(name, value);
                }
            }
            builder
                .body(Body::from_stream(body_rx))
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
        }
        Ok(Err(e)) => (StatusCode::BAD_GATEWAY, e).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "session task dropped before responding",
        )
            .into_response(),
    }
}

/// Runs the Proxy-TLS prover protocol for one session:
/// commit → TLS via verifier relay → author HTTP request → stream response →
/// reveal (minus redactions) → prove → close.
#[instrument(skip_all, fields(session = %req.session_id))]
async fn run_session(
    state: &Arc<AppState>,
    req: SessionRequest,
    io: impl futures::AsyncRead + futures::AsyncWrite + Send + Unpin + 'static,
    ws_wait: Duration,
    head_tx: oneshot::Sender<Result<RespHead, String>>,
    mut body_tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
) -> Result<()> {
    let t0 = Instant::now();
    let max_recv = req.max_recv_bytes.unwrap_or(state.config.default_max_recv_bytes);

    // --- tlsn session with the verifier over the mux (channel B) ---
    let session = Session::new(io);
    let (driver, mut handle) = session.split();
    let driver_task = tokio::spawn(driver);

    let prover = handle
        .new_prover(ProverConfig::builder().build()?)
        .map_err(|e| anyhow!("new_prover: {e}"))?
        .commit(
            ProxyTlsConfig::builder()
                .server_name(
                    DnsName::try_from(req.server_name.as_str())
                        .context("invalid server_name")?,
                )
                .build()?,
        )
        .await
        .map_err(|e| anyhow!("commit: {e}"))?;

    // Proxy mode: TLS runs through the verifier's relay (channel C). proverd
    // never touches the upstream network itself.
    let (tls_connection, prover) = prover
        .connect(
            TlsClientConfig::builder()
                .server_name(ServerName::Dns(
                    DnsName::try_from(req.server_name.as_str())?,
                ))
                .root_store(state.config.root_store.clone())
                .build()?,
        )
        .map_err(|e| anyhow!("tls connect setup: {e}"))?;
    let tls_connection = TokioIo::new(tls_connection.compat());

    let prover_task = tokio::spawn(prover.into_future());
    let t_tls = t0.elapsed();
    debug!("tls session established via relay");

    // --- author the HTTP request ---
    let (mut request_sender, connection) = hyper::client::conn::http1::handshake(tls_connection)
        .await
        .context("http1 handshake")?;
    tokio::spawn(connection);

    let mut builder = hyper::Request::builder()
        .method(req.request.method.as_str())
        .uri(&req.request.path)
        .header("Host", &req.server_name)
        .header("Connection", "close");
    for (name, value) in &req.request.headers {
        builder = builder.header(name, value);
    }
    let request = builder
        .body(Full::new(Bytes::from(req.request.body.clone())))?;

    let response = request_sender
        .send_request(request)
        .await
        .context("send_request")?;
    let t_ttfb = t0.elapsed();

    // Forward status + headers to the POST handler so it can respond
    // immediately, then stream body chunks as they arrive.
    let head = RespHead {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .filter(|(name, _)| {
                // Framing is re-done by axum; drop hop-by-hop headers.
                !matches!(
                    name.as_str(),
                    "content-length" | "transfer-encoding" | "connection"
                )
            })
            .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect(),
    };
    if head_tx.send(Ok(head)).is_err() {
        return Err(anyhow!("POST handler went away before response head"));
    }

    let (mut response, mut recv_bytes) = (response, 0usize);
    let mut body_result: Result<(), String> = Ok(());
    while let Some(frame) = response.body_mut().frame().await {
        match frame {
            Ok(frame) => {
                if let Some(data) = frame.data_ref() {
                    recv_bytes += data.len();
                    if recv_bytes > max_recv {
                        body_result = Err(format!(
                            "response exceeded max_recv_bytes ({max_recv})"
                        ));
                        break;
                    }
                    let chunk = Bytes::copy_from_slice(data);
                    if body_tx.send(Ok(chunk)).await.is_err() {
                        return Err(anyhow!("POST handler went away mid-body"));
                    }
                }
            }
            Err(e) => {
                body_result = Err(format!("upstream body error: {e}"));
                break;
            }
        }
    }
    let t_transfer = t0.elapsed();
    drop(body_tx);

    if let Err(e) = &body_result {
        // Still let the proof protocol unwind below via `?` after closing.
        return Err(anyhow!(e.clone()));
    }

    // --- TLS session done; recover the prover ---
    let mut prover = prover_task
        .await
        .context("prover task join")?
        .map_err(|e| anyhow!("prover future: {e}"))?;

    // --- selective disclosure: reveal everything except redacted header
    // values in the sent transcript ---
    let mut prove_builder = ProveConfig::builder(prover.transcript());
    prove_builder.server_identity();

    let sent = prover.transcript().sent();
    let redacted = header_value_ranges(sent, &req.redact);
    for range in complement_ranges(sent.len(), &redacted) {
        prove_builder
            .reveal_sent(&range)
            .map_err(|e| anyhow!("reveal_sent {range:?}: {e}"))?;
    }
    prove_builder
        .reveal_recv_all()
        .map_err(|e| anyhow!("reveal_recv_all: {e}"))?;
    let prove_config = prove_builder
        .build()
        .map_err(|e| anyhow!("prove config: {e}"))?;

    prover
        .prove(&prove_config)
        .await
        .map_err(|e| anyhow!("prove: {e}"))?;
    let sent_len = prover.transcript().sent().len();
    prover.close().await.map_err(|e| anyhow!("close: {e}"))?;

    let t_proof = t0.elapsed();

    handle.close();
    driver_task.await.context("driver join")??;

    let timings = SessionTimings {
        ws_wait_ms: ws_wait.as_millis(),
        tls_setup_ms: (t_tls - ws_wait).as_millis(),
        ttfb_ms: (t_ttfb - t_tls).as_millis(),
        transfer_ms: (t_transfer - t_ttfb).as_millis(),
        proof_ms: (t_proof - t_transfer).as_millis(),
        total_ms: t0.elapsed().as_millis(),
        sent_bytes: sent_len,
        recv_bytes,
    };
    info!(?timings, "session complete");
    state.set_status(&req.session_id, SessionStatus::Complete(timings));
    Ok(())
}

/// Finds the byte ranges of header VALUES for `header_names` in a raw HTTP/1.1
/// message (case-insensitive header names).
fn header_value_ranges(buf: &[u8], header_names: &[String]) -> Vec<Range<usize>> {
    let lower: Vec<u8> = buf.iter().map(|b| b.to_ascii_lowercase()).collect();
    let mut ranges = Vec::new();
    for name in header_names {
        let needle = format!("\r\n{}:", name.to_ascii_lowercase());
        let needle = needle.as_bytes();
        let mut start = 0;
        while start + needle.len() <= lower.len() {
            let Some(pos) = lower[start..]
                .windows(needle.len())
                .position(|w| w == needle)
                .map(|p| p + start)
            else {
                break;
            };
            let mut value_start = pos + needle.len();
            while value_start < buf.len()
                && (buf[value_start] == b' ' || buf[value_start] == b'\t')
            {
                value_start += 1;
            }
            let value_end = lower[value_start..]
                .windows(2)
                .position(|w| w == b"\r\n")
                .map(|p| value_start + p)
                .unwrap_or(buf.len());
            ranges.push(value_start..value_end);
            start = value_end;
        }
    }
    ranges.sort_by_key(|r| r.start);
    ranges
}

/// Complement of `excluded` within `0..len`.
fn complement_ranges(len: usize, excluded: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for r in excluded {
        if r.start > cursor {
            out.push(cursor..r.start);
        }
        cursor = cursor.max(r.end);
    }
    if cursor < len {
        out.push(cursor..len);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_ranges() {
        let req = b"GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer secret-token\r\nAccept: */*\r\n\r\n";
        let ranges = header_value_ranges(req, &["authorization".to_string()]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&req[ranges[0].clone()], b"Bearer secret-token");
    }

    #[test]
    fn complements() {
        assert_eq!(complement_ranges(10, &[2..5]), vec![0..2, 5..10]);
        assert_eq!(complement_ranges(10, &[]), vec![0..10]);
        assert_eq!(complement_ranges(10, &[0..10]), Vec::<Range<usize>>::new());
    }
}
