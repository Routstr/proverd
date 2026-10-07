//! TLS test server: a chat-shaped, OpenAI-compatible mock upstream, served
//! over TLS with the fixture certificates (domain `test-server.io`).
//!
//! Used by the integration tests and exposed as the `mock_upstream` example,
//! so this repo is self-contained (no external fixture crate).
//!
//! Routes (all require `authorization: Bearer random_auth_token`):
//! - `POST /v1/chat/completions` — canned completion; SSE when `stream: true`
//!   (usage chunk when `stream_options.include_usage`).
//! - `GET /protected` — small auth-gated JSON document.

use std::{convert::Infallible, net::SocketAddr, sync::Arc};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures_rustls::{
    TlsAcceptor,
    pki_types::{CertificateDer, PrivateKeyDer},
    rustls::{RootCertStore, ServerConfig, server::WebPkiClientVerifier},
};
use hyper::{Request, body::Incoming, server::conn::http1};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tower_service::Service;

use crate::fixtures::{
    AUTH_TOKEN, CA_CERT_DER, SERVER_CERT_DER, SERVER_DOMAIN, SERVER_KEY_DER,
};

#[derive(Clone)]
struct AppState;

/// Install the ring crypto provider (idempotent).
pub fn ensure_crypto_provider() {
    let _ = futures_rustls::rustls::crypto::ring::default_provider().install_default();
}

/// The fixture axum app.
pub fn app() -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/protected", get(protected))
        .with_state(AppState)
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == format!("Bearer {AUTH_TOKEN}"))
        .unwrap_or(false)
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": {"message": "Invalid or missing token", "type": "invalid_request_error"}
        })),
    )
        .into_response()
}

async fn protected(headers: HeaderMap) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    Json(serde_json::json!({"secret": "fixture-data", "ok": true})).into_response()
}

async fn chat_completions(headers: HeaderMap, State(_): State<AppState>, body: String) -> Response {
    if !authorized(&headers) {
        return unauthorized();
    }
    let body: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": {"message": "invalid JSON body"}})),
            )
                .into_response();
        }
    };
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or("mock-model")
        .to_string();
    let first_user_msg = body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        })
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let reply = format!("Mock reply to: {first_user_msg}");

    let streaming = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let include_usage = body
        .get("stream_options")
        .and_then(|o| o.get("include_usage"))
        .and_then(|u| u.as_bool())
        .unwrap_or(false);

    if streaming {
        let mid = reply.len() / 2;
        let part1 = reply[..mid].to_string();
        let part2 = reply[mid..].to_string();
        let usage =
            serde_json::json!({"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19});

        let chunk = |delta: serde_json::Value, finish: Option<&str>| {
            serde_json::json!({
                "id": "chatcmpl-mock1", "object": "chat.completion.chunk",
                "created": 1730000000, "model": model,
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
            })
        };

        let mut events: Vec<Result<Event, Infallible>> = vec![
            Ok(Event::default().data(
                chunk(serde_json::json!({"role": "assistant", "content": part1}), None)
                    .to_string(),
            )),
            Ok(Event::default().data(
                chunk(serde_json::json!({"content": part2}), Some("stop")).to_string(),
            )),
        ];
        if include_usage {
            events.push(Ok(Event::default().data(
                serde_json::json!({
                    "id": "chatcmpl-mock1", "object": "chat.completion.chunk",
                    "created": 1730000000, "model": model,
                    "choices": [], "usage": usage,
                })
                .to_string(),
            )));
        }
        events.push(Ok(Event::default().data("[DONE]")));

        Sse::new(futures::stream::iter(events))
            .keep_alive(KeepAlive::default())
            .into_response()
    } else {
        Json(serde_json::json!({
            "id": "chatcmpl-mock1", "object": "chat.completion", "created": 1730000000,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": reply},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19}
        }))
        .into_response()
    }
}

/// Serves one TLS connection (TLS 1.2/1.3 accept + HTTP/1.1, no keep-alive).
pub async fn bind<T: futures::AsyncRead + futures::AsyncWrite + Send + Unpin + 'static>(
    socket: T,
) -> anyhow::Result<()> {
    ensure_crypto_provider();

    let key = PrivateKeyDer::Pkcs8(SERVER_KEY_DER.into());
    let cert = CertificateDer::from(SERVER_CERT_DER);
    let mut root_store = RootCertStore::empty();
    root_store.add(CA_CERT_DER.into()).unwrap();
    let client_verifier = WebPkiClientVerifier::builder(root_store.into())
        .allow_unauthenticated()
        .build()
        .unwrap();
    let config = ServerConfig::builder()
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![cert], key)?;
    let acceptor = TlsAcceptor::from(Arc::new(config));

    let conn = acceptor.accept(socket).await?;
    let io = TokioIo::new(conn.compat());
    let app = app();
    let service =
        hyper::service::service_fn(move |request: Request<Incoming>| app.clone().call(request));
    http1::Builder::new()
        .keep_alive(false)
        .serve_connection(io, service)
        .await
        .map_err(|e| anyhow::anyhow!(e))
}

/// Binds `127.0.0.1:port` and serves the fixture app until the process ends.
/// Returns the bound address (port 0 picks a free port).
pub async fn serve(port: u16) -> std::io::Result<SocketAddr> {
    ensure_crypto_provider();
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                continue;
            };
            tokio::spawn(async move {
                if let Err(e) = bind(socket.compat()).await {
                    tracing::warn!(
                        server = SERVER_DOMAIN,
                        "fixture tls connection failed: {e}"
                    );
                }
            });
        }
    });
    Ok(addr)
}
