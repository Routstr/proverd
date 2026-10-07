//! Native TLSN test verifier (library form).
//!
//! Used by the M1 e2e test, the `test_verifier` CLI example, and external
//! integration harnesses (routstr-core pytest, M5 e2e). Connects channel B
//! (WebSocket to proverd) and channel C (raw TCP to the upstream it chooses
//! to dial), runs the proxy-mode verifier, and returns the verified output.

use std::{net::SocketAddr, time::Instant};

use anyhow::{Context, Result, anyhow};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncReadCompatExt;

use tlsn::{
    Session,
    config::verifier::VerifierConfig,
    connection::ServerName,
    transcript::PartialTranscript,
    verifier::{VerifierCommitStart, VerifierOutput},
    webpki::{CertificateDer, RootCertStore},
};

use crate::ws_io::{MsgKind, WsMsg, WsPipe};

/// Newtype around tungstenite messages so we can implement `WsMsg`
/// (orphan rule).
struct Tng(tokio_tungstenite::tungstenite::Message);

impl WsMsg for Tng {
    fn kind(self) -> MsgKind {
        use tokio_tungstenite::tungstenite::Message;
        match self.0 {
            Message::Binary(data) => MsgKind::Payload(data.to_vec()),
            Message::Close(_) => MsgKind::Close,
            _ => MsgKind::Skip,
        }
    }

    fn from_bytes(data: Vec<u8>) -> Self {
        Tng(tokio_tungstenite::tungstenite::Message::Binary(data.into()))
    }
}

fn ws_client_io(
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
) -> impl futures::AsyncRead + futures::AsyncWrite + Send + Unpin + 'static {
    let (sink, stream) = ws.split();
    let stream = stream.map(|r| {
        r.map(Tng)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))
    });
    // `future::ready` keeps the `With` adapter Unpin (an async block wouldn't be).
    let sink = sink.with(|t: Tng| {
        futures::future::ready(Ok::<_, tokio_tungstenite::tungstenite::Error>(t.0))
    });
    async_io_stream::IoStream::new(WsPipe::new(stream, sink))
}

/// Output of a successful verification.
pub struct VerificationResult {
    /// Verified upstream server name.
    pub server_name: String,
    /// Disclosed transcript (redacted ranges read as zero bytes).
    pub transcript: PartialTranscript,
    /// Wall-clock timings of the verifier side.
    pub commit_ms: u128,
    pub proof_ms: u128,
}

/// Where the verifier dials channel C.
pub enum DialTarget {
    /// Raw socket address (tests/fixtures).
    Addr(SocketAddr),
    /// DNS name + port, resolved after the commit reveals the server name
    /// (which must match this name).
    Host(String, u16),
}

/// Runs the native proxy-mode verifier against a running proverd.
///
/// * `ws_url` — full mux WebSocket URL, including `?session_id=...`.
/// * `dial` — where the verifier dials for channel C.
/// * `extra_roots` — extra root certificates (e.g. a fixture CA).
/// * `on_ready` — notified with the committed server name.
pub async fn run_verifier(
    ws_url: &str,
    dial: DialTarget,
    extra_roots: Vec<CertificateDer>,
    on_ready: Option<tokio::sync::oneshot::Sender<String>>,
) -> Result<VerificationResult> {
    let t0 = Instant::now();

    // Channel B.
    let (ws, _) = tokio_tungstenite::connect_async(ws_url)
        .await
        .context("ws connect to proverd")?;
    let io = ws_client_io(ws);

    let session = Session::new(io);
    let (driver, mut handle) = session.split();
    let driver_task = tokio::spawn(driver);

    let mut roots = RootCertStore::mozilla().roots;
    roots.extend(extra_roots);
    let verifier_config = VerifierConfig::builder()
        .root_store(RootCertStore { roots })
        .build()?;
    let verifier = handle
        .new_verifier(verifier_config)
        .map_err(|e| anyhow!("new_verifier: {e}"))?;

    let verifier = match verifier
        .commit()
        .await
        .map_err(|e| anyhow!("commit: {e}"))?
    {
        VerifierCommitStart::Proxy(verifier) => {
            let committed_name = verifier.config().server_name().to_string();
            // The committed server name is known now: report it (policy
            // checks) before relaying anything.
            if let Some(tx) = on_ready {
                let _ = tx.send(committed_name.clone());
            }
            let upstream_addr = match &dial {
                DialTarget::Addr(addr) => *addr,
                DialTarget::Host(host, port) => {
                    if host != &committed_name {
                        return Err(anyhow!(
                            "committed server name {committed_name} != expected {host}"
                        ));
                    }
                    tokio::net::lookup_host((host.as_str(), *port))
                        .await
                        .context("upstream DNS lookup")?
                        .next()
                        .ok_or_else(|| anyhow!("no addresses for {host}"))?
                }
            };
            // Channel C: the verifier itself dials the upstream and relays
            // ciphertext between it and the prover.
            let server_socket = TcpStream::connect(upstream_addr).await?;
            server_socket.set_nodelay(true)?;
            verifier
                .accept()
                .await
                .map_err(|e| anyhow!("accept: {e}"))?
                .run(server_socket.compat())
                .await
                .map_err(|e| anyhow!("run: {e}"))?
        }
        VerifierCommitStart::Mpc(_) => {
            return Err(anyhow!("expected proxy mode, prover requested mpc"));
        }
    };
    let commit_ms = t0.elapsed().as_millis();

    let verifier = verifier.verify().await.map_err(|e| anyhow!("verify: {e}"))?;
    if !verifier.request().server_identity() {
        verifier
            .reject(Some("expected server identity reveal"))
            .await
            .map_err(|e| anyhow!("reject: {e}"))?;
        return Err(anyhow!("prover did not reveal server identity"));
    }
    let (
        VerifierOutput {
            server_name,
            transcript,
            ..
        },
        verifier,
    ) = verifier.accept().await.map_err(|e| anyhow!("accept proof: {e}"))?;
    verifier.close().await.map_err(|e| anyhow!("close: {e}"))?;

    handle.close();
    driver_task.await.context("driver join")??;

    let proof_ms = t0.elapsed().as_millis() - commit_ms;

    let server_name = server_name.context("no server name revealed")?;
    let ServerName::Dns(server_name) = server_name;
    let transcript = transcript.context("no transcript revealed")?;
    Ok(VerificationResult {
        server_name: server_name.to_string(),
        transcript,
        commit_ms,
        proof_ms,
    })
}

/// Render redacted (zero) bytes as `🙈` for display.
pub fn redacted_string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace('\0', "🙈")
}
