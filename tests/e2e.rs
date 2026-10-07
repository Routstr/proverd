//! M1 end-to-end: proverd (Proxy-TLS prover) + native Rust tlsn verifier
//! against a local TLS fixture server.
//!
//! Topology mirrors production:
//! - channel B: verifier → proverd `/ws` (real WebSocket, tlsn mux)
//! - channel C: verifier → fixture server (raw TCP relay of ciphertext)
//! - API: test client → proverd `POST /sessions` (decrypted response)
//!


use std::{net::SocketAddr, time::Instant};

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};

use tlsn::webpki::{CertificateDer, RootCertStore};

use proverd::fixtures::{AUTH_TOKEN, CA_CERT_DER, SERVER_DOMAIN};
use proverd::{
    AppState, Config, SessionStatus, router,
    testfixture,
    testverifier::{DialTarget, run_verifier},
};

/// Spawn the in-repo TLS fixture server on an ephemeral port.
async fn spawn_fixture() -> SocketAddr {
    testfixture::serve(0).await.unwrap()
}

/// Spawn proverd trusting only the fixture CA.
async fn spawn_proverd() -> (SocketAddr, std::sync::Arc<AppState>) {
    let config = Config {
        root_store: RootCertStore {
            roots: vec![CertificateDer(CA_CERT_DER.to_vec())],
        },
        ..Config::default()
    };
    let state = AppState::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_state = state.clone();
    tokio::spawn(async move {
        axum::serve(listener, router(serve_state)).await.unwrap();
    });
    (addr, state)
}

/// POST /sessions to proverd and read the full (streamed) response body.
async fn post_session(proverd_addr: SocketAddr, session_id: &str, path: &str) -> Result<(u16, Vec<u8>)> {
    let tcp = TcpStream::connect(proverd_addr).await?;
    tcp.set_nodelay(true)?;
    let io = TokioIo::new(tcp);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
    tokio::spawn(conn);

    let body = serde_json::json!({
        "session_id": session_id,
        "server_name": SERVER_DOMAIN,
        "port": 443,
        "request": {
            "method": "GET",
            "path": path,
            "headers": [["authorization", format!("Bearer {AUTH_TOKEN}")]],
        },
        "redact": ["authorization"],
        "max_recv_bytes": 65536
    });
    let req = hyper::Request::builder()
        .method("POST")
        .uri(format!("http://{proverd_addr}/sessions"))
        .header("content-type", "application/json")
        .body(Full::new(bytes::Bytes::from(serde_json::to_vec(&body)?)))?;

    let resp = sender.send_request(req).await?;
    let status = resp.status().as_u16();
    let body = resp.into_body().collect().await?.to_bytes().to_vec();
    Ok((status, body))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn proxy_tls_e2e() -> Result<()> {
    let fixture_addr = spawn_fixture().await;
    let (proverd_addr, state) = spawn_proverd().await;
    eprintln!("fixture={fixture_addr} proverd={proverd_addr}");

    let session_id = "test-session-1";

    // Verifier dials out on channels B and C.
    let proverd_ws = format!("ws://{proverd_addr}");
    let session_id_owned = session_id.to_string();
    let verifier_task = tokio::spawn(async move {
        run_verifier(
            &format!("{proverd_ws}/ws?session_id={session_id_owned}"),
            DialTarget::Addr(fixture_addr),
            vec![CertificateDer(CA_CERT_DER.to_vec())],
            None,
        )
        .await
    });

    // API call (channel A analogue): routstr-core → proverd.
    let t0 = Instant::now();
    let (status, api_body) = post_session(proverd_addr, session_id, "/protected").await?;
    eprintln!("[api] POST /sessions -> {status}, {} bytes in {:?}", api_body.len(), t0.elapsed());
    assert_eq!(status, 200, "proverd POST failed: {}", String::from_utf8_lossy(&api_body));

    let result = verifier_task.await.context("verifier join")??;
    assert_eq!(result.server_name, SERVER_DOMAIN);
    let transcript = result.transcript;

    // --- Check disclosed request ---
    let sent = transcript
        .sent_unsafe(transcript.len_sent())
        .context("sent_unsafe")?;
    let sent_str = String::from_utf8_lossy(&sent);
    eprintln!("[verifier] disclosed request:\n{}", sent_str.replace('\0', "🙈"));
    assert!(sent_str.contains("GET /protected"), "request line missing");
    assert!(
        sent_str.to_ascii_lowercase().contains("authorization:"),
        "authorization header must be present (value redacted)"
    );
    assert!(
        !sent_str.contains(AUTH_TOKEN),
        "REDACTION FAILURE: authorization value was revealed"
    );

    // --- Check disclosed response against what the API call returned ---
    let received = transcript
        .received_unsafe(transcript.len_received())
        .context("received_unsafe")?;
    let split = received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("no header/body split in received transcript")?;
    let proven_body = &received[split + 4..];
    eprintln!("[verifier] disclosed response body ({} bytes)", proven_body.len());
    assert_eq!(
        proven_body, api_body.as_slice(),
        "proven upstream body != body returned by proverd API"
    );

    // --- Timings from proverd (verifier finishes slightly before the
    // prover's close handshake; poll briefly) ---
    let mut status = state.status(session_id);
    for _ in 0..50 {
        if matches!(status, Some(SessionStatus::Complete(_))) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        status = state.status(session_id);
    }
    match status {
        Some(SessionStatus::Complete(t)) => {
            eprintln!(
                "[proverd] timings: ws_wait={}ms tls_setup={}ms ttfb={}ms transfer={}ms proof={}ms total={}ms (sent={}B recv={}B)",
                t.ws_wait_ms, t.tls_setup_ms, t.ttfb_ms, t.transfer_ms, t.proof_ms, t.total_ms, t.sent_bytes, t.recv_bytes
            );
        }
        other => panic!("expected Complete status, got {other:?}"),
    }

    eprintln!("M1 e2e: VERIFIED ✓ {SERVER_DOMAIN}");
    Ok(())
}
